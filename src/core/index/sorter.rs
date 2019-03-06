use core::index::merge_state::{LiveDocsDocMap, ReaderWrapperEnum};
use core::index::{LeafReader, NumericDocValues, NumericDocValuesRef};
use core::search::field_comparator::{ComparatorValue, FieldComparator};
use core::search::sort::Sort;
use core::search::sort_field::{SortField, SortFieldType, SortedNumericSelector};
use core::util::packed::{PackedLongValuesBuilder, PackedLongValuesBuilderType, DEFAULT_PAGE_SIZE};
use core::util::packed_misc::COMPACT;
use core::util::{BitsRef, DocId};

use error::ErrorKind::IllegalArgument;
use error::Result;

use std::cmp::Ordering;
use std::collections::BinaryHeap;

/// Sorts documents of a given index by returning a permutation
/// on the document IDs.
pub struct Sorter {
    sort: Sort,
}

impl Sorter {
    pub fn new(sort: Sort) -> Self {
        debug_assert!(!sort.needs_scores());
        Sorter { sort }
    }

    /// Check consistency of a `SorterDocMap`, useful for assertions.
    pub fn is_consistent(doc_map: &SorterDocMap) -> bool {
        let max_doc = doc_map.len() as i32;
        for i in 0..max_doc {
            let new_id = doc_map.old_to_new(i);
            let old_id = doc_map.new_to_old(new_id);
            assert!(new_id >= 0 && new_id < max_doc);
            assert_eq!(i, old_id);
        }
        true
    }

    /// Computes the old-to-new permutation over the given comparator.
    fn sort(
        max_doc: DocId,
        comparator: &mut SorterDocComparator,
    ) -> Result<Option<PackedLongDocMap>> {
        debug_assert!(max_doc > 0);
        // check if the index is sorted
        let mut sorted = true;
        for i in 1..max_doc {
            if comparator.compare(i - 1, i)? == Ordering::Greater {
                sorted = false;
                break;
            }
        }

        if sorted {
            return Ok(None);
        }

        // sort doc IDs
        let mut docs = vec![0i32; max_doc as usize];
        for i in 0..max_doc {
            docs[i as usize] = i;
        }

        let mut sort_res = Ok(());
        docs.sort_by(|doc1, doc2| match comparator.compare(*doc1, *doc2) {
            Err(e) => {
                sort_res = Err(e);
                Ordering::Equal
            }
            Ok(o) => o,
        });
        if let Err(e) = sort_res {
            return Err(e);
        }

        // The reason why we use MonotonicAppendingLongBuffer here is that it
        // wastes very little memory if the index is in random order but can save
        // a lot of memory if the index is already "almost" sorted
        let mut new_to_old_builder = PackedLongValuesBuilder::new(
            DEFAULT_PAGE_SIZE,
            COMPACT,
            PackedLongValuesBuilderType::Monotonic,
        );
        for i in 0..max_doc as usize {
            new_to_old_builder.add(docs[i] as i64);
        }
        // NOTE: the #build method contain reference, but the builder will move after return,
        // so we won't use the build result
        new_to_old_builder.build();

        // invert the docs mapping
        for i in 0..max_doc {
            docs[new_to_old_builder.get(i)? as usize] = i;
        } // docs is now the old_to_new mapping

        let mut old_to_new_builder = PackedLongValuesBuilder::new(
            DEFAULT_PAGE_SIZE,
            COMPACT,
            PackedLongValuesBuilderType::Monotonic,
        );
        for i in 0..max_doc as usize {
            old_to_new_builder.add(docs[i] as i64);
        }
        // NOTE: the #build method contain reference, but the builder will move after return,
        // so we won't use the build result
        old_to_new_builder.build();

        Ok(Some(PackedLongDocMap {
            max_doc: max_doc as usize,
            old_to_new: old_to_new_builder,
            new_to_old: new_to_old_builder,
        }))
    }

    /// Returns a mapping from the old document ID to its new location in the
    /// sorted index. Implementations can use the auxiliary
    /// {@link #sort(int, DocComparator)} to compute the old-to-new permutation
    /// given a list of documents and their corresponding values.
    ///
    /// A return value of `None` is allowed and means that
    /// <code>reader</code> is already sorted.
    ///
    /// NOTE: deleted documents are expected to appear in the mapping as
    /// well, they will however be marked as deleted in the sorted view.
    pub fn sort_leaf_reader(&self, reader: &LeafReader) -> Result<Option<PackedLongDocMap>> {
        let fields = self.sort.get_sort();
        let mut reverses = Vec::with_capacity(fields.len());
        let mut comparators = Vec::with_capacity(fields.len());
        for i in 0..fields.len() {
            reverses.push(fields[i].is_reverse());
            let mut comparator = fields[i].get_comparator(1, None);
            comparator.get_information_from_reader(reader)?;
            comparators.push(comparator);
        }
        let mut comparator = SortFieldsDocComparator {
            comparators,
            reverses,
        };
        Self::sort(reader.max_doc(), &mut comparator)
    }

    pub fn get_or_wrap_numeric(
        reader: &LeafReader,
        sort_field: &SortField,
    ) -> Result<NumericDocValuesRef> {
        match sort_field {
            SortField::SortedNumeric(s) => SortedNumericSelector::wrap(
                reader.get_sorted_numeric_doc_values(sort_field.field())?,
                s.selector(),
                s.numeric_type(),
            ),
            _ => reader.get_numeric_doc_values(sort_field.field()),
        }
    }
}

pub struct PackedLongDocMap {
    max_doc: usize,
    old_to_new: PackedLongValuesBuilder,
    new_to_old: PackedLongValuesBuilder,
}

impl SorterDocMap for PackedLongDocMap {
    fn old_to_new(&self, doc_id: DocId) -> DocId {
        self.old_to_new.get(doc_id).unwrap() as DocId
    }

    fn new_to_old(&self, doc_id: i32) -> i32 {
        self.new_to_old.get(doc_id).unwrap() as DocId
    }

    fn len(&self) -> usize {
        self.max_doc
    }
}

/// A permutation of doc IDs. For every document ID between <tt>0</tt> and
/// `IndexReader#max_doc()`, `old_to_new(new_to_old(doc_id))` must
/// return `doc_id`
pub trait SorterDocMap {
    /// Given a doc ID from the original index, return its ordinal in the
    /// sorted index
    fn old_to_new(&self, doc_id: DocId) -> DocId;

    /// Given the ordinal of a doc ID, return its doc ID in the original index.
    fn new_to_old(&self, doc_id: DocId) -> DocId;

    /// Return the number of documents in this map. This must be equal to the
    /// `LeafReader#max_doc()` number of documents of the `LeafReader` which
    /// is sorted.
    fn len(&self) -> usize;
}

/// a comparator of doc IDs
trait SorterDocComparator {
    fn compare(&mut self, doc1: DocId, doc2: DocId) -> Result<Ordering>;
}

struct SortFieldsDocComparator {
    comparators: Vec<Box<FieldComparator>>,
    reverses: Vec<bool>,
}

impl SorterDocComparator for SortFieldsDocComparator {
    fn compare(&mut self, doc1: i32, doc2: i32) -> Result<Ordering> {
        for i in 0..self.comparators.len() {
            // TODO: would be better if copy() didnt cause a term lookup in TermOrdVal & co,
            // the segments are always the same here...
            self.comparators[i].copy(0, ComparatorValue::Doc(doc1))?;
            self.comparators[i].set_bottom(0);
            let mut comp = self.comparators[i].compare_bottom(ComparatorValue::Doc(doc2))?;
            if !self.reverses[i] {
                comp = comp.reverse();
            }
            if comp != Ordering::Equal {
                return Ok(comp);
            }
        }
        Ok(doc1.cmp(&doc2))
    }
}

pub struct MultiSorter;

impl MultiSorter {
    /// Does a merge sort of the leaves of the incoming reader, returning `DocMap`
    /// to map each leaf's documents into the merged segment.  The documents for
    /// each incoming leaf reader must already be sorted by the same sort!
    /// Returns null if the merge sort is not needed (segments are already in index sort order).
    pub fn sort(sort: &Sort, readers: &[ReaderWrapperEnum]) -> Result<Vec<LiveDocsDocMap>> {
        let fields = sort.get_sort();

        let mut comparators = Vec::with_capacity(fields.len());
        for field in fields {
            comparators.push(Self::get_comparator(readers, field)?);
        }

        let leaf_count = readers.len();

        let mut queue = BinaryHeap::with_capacity(leaf_count);
        let mut builders = Vec::with_capacity(leaf_count);

        for i in 0..leaf_count {
            queue.push(LeafAndDocId::new(
                i,
                readers[i].live_docs(),
                readers[i].max_doc(),
                &comparators,
            ));
            builders.push(PackedLongValuesBuilder::new(
                DEFAULT_PAGE_SIZE,
                COMPACT,
                PackedLongValuesBuilderType::Monotonic,
            ));
        }

        let mut mapped_doc_id = 0;
        let mut last_reader_index = 0;
        let mut sorted = true;
        loop {
            let mut tmp = None;
            {
                if let Some(mut top) = queue.pop() {
                    if last_reader_index > top.reader_index {
                        // merge sort is needed
                        sorted = false;
                    }
                    last_reader_index = top.reader_index;
                    builders[last_reader_index].add(mapped_doc_id);
                    if top.live_docs.get(top.doc_id as usize)? {
                        mapped_doc_id += 1;
                    }
                    top.doc_id += 1;
                    if top.doc_id < top.max_doc {
                        tmp = Some(top);
                    }
                } else {
                    break;
                }
            }
            if tmp.is_some() {
                queue.push(tmp.unwrap());
            }
        }

        if sorted {
            return Ok(Vec::with_capacity(0));
        }

        let mut i = 0;
        let mut doc_maps = Vec::with_capacity(leaf_count);
        for mut builder in builders {
            builder.build();
            let live_docs = readers[i].live_docs();
            doc_maps.push(LiveDocsDocMap::new(live_docs, builder, 0));
            i += 1;
        }

        Ok(doc_maps)
    }

    /// Returns {@code CrossReaderComparator} for the provided readers to represent
    /// the requested {@link SortField} sort order.
    fn get_comparator(
        readers: &[ReaderWrapperEnum],
        sort_field: &SortField,
    ) -> Result<CrossReaderComparatorEnum> {
        let reverse = sort_field.is_reverse();
        let field_type = sort_field.field_type();
        match field_type {
            SortFieldType::String => unimplemented!(),
            SortFieldType::Long | SortFieldType::Int => {
                let mut values = Vec::with_capacity(readers.len());
                let mut docs_with_fields = Vec::with_capacity(readers.len());
                for reader in readers {
                    values.push(Sorter::get_or_wrap_numeric(reader, sort_field)?);
                    docs_with_fields.push(reader.get_docs_with_field(sort_field.field())?);
                }
                let missing_value = if let Some(missing) = sort_field.missing_value() {
                    if field_type == SortFieldType::Long {
                        missing.get_long().unwrap()
                    } else if field_type == SortFieldType::Int {
                        missing.get_int().unwrap() as i64
                    } else {
                        unreachable!()
                    }
                } else {
                    0
                };
                Ok(CrossReaderComparatorEnum::Long(
                    LongCrossReaderComparator::new(
                        docs_with_fields,
                        values,
                        missing_value,
                        reverse,
                    ),
                ))
            }
            SortFieldType::Double | SortFieldType::Float => {
                let mut values = Vec::with_capacity(readers.len());
                let mut docs_with_fields = Vec::with_capacity(readers.len());
                for reader in readers {
                    values.push(Sorter::get_or_wrap_numeric(reader, sort_field)?);
                    docs_with_fields.push(reader.get_docs_with_field(sort_field.field())?);
                }
                let missing_value = if let Some(missing) = sort_field.missing_value() {
                    if field_type == SortFieldType::Double {
                        missing.get_double().unwrap()
                    } else if field_type == SortFieldType::Float {
                        missing.get_float().unwrap() as f64
                    } else {
                        unreachable!()
                    }
                } else {
                    0.0
                };
                Ok(CrossReaderComparatorEnum::Double(
                    DoubleCrossReaderComparator::new(
                        docs_with_fields,
                        values,
                        missing_value,
                        reverse,
                    ),
                ))
            }
            _ => bail!(IllegalArgument(format!(
                "unhandled SortField.getType()={:?}",
                field_type
            ))),
        }
    }
}

struct LeafAndDocId<'a> {
    reader_index: usize,
    live_docs: BitsRef,
    max_doc: i32,
    doc_id: DocId,
    comparators: &'a [CrossReaderComparatorEnum],
}

impl<'a> LeafAndDocId<'a> {
    fn new(
        reader_index: usize,
        live_docs: BitsRef,
        max_doc: i32,
        comparators: &'a [CrossReaderComparatorEnum],
    ) -> Self {
        LeafAndDocId {
            reader_index,
            live_docs,
            max_doc,
            comparators,
            doc_id: 0,
        }
    }
}

impl<'a> Eq for LeafAndDocId<'a> {}

impl<'a> PartialEq for LeafAndDocId<'a> {
    fn eq(&self, other: &LeafAndDocId) -> bool {
        self.reader_index == other.reader_index && self.doc_id == other.doc_id
    }
}

impl<'a> Ord for LeafAndDocId<'a> {
    // reverse ord for BinaryHeap
    fn cmp(&self, other: &Self) -> Ordering {
        for comparator in self.comparators {
            let cmp = comparator
                .compare(
                    other.reader_index,
                    other.doc_id,
                    self.reader_index,
                    self.doc_id,
                )
                .unwrap();
            if cmp != Ordering::Equal {
                return cmp.reverse();
            }
        }
        // tie-break by doc_id natural order:
        if self.reader_index != other.reader_index {
            self.reader_index.cmp(&other.reader_index)
        } else {
            self.doc_id.cmp(&other.doc_id)
        }
    }
}

impl<'a> PartialOrd for LeafAndDocId<'a> {
    fn partial_cmp(&self, other: &LeafAndDocId) -> Option<Ordering> {
        Some(other.cmp(self))
    }
}

enum CrossReaderComparatorEnum {
    Long(LongCrossReaderComparator),
    Double(DoubleCrossReaderComparator),
}

impl CrossReaderComparator for CrossReaderComparatorEnum {
    fn compare(
        &self,
        reader_index1: usize,
        doc_id1: DocId,
        reader_index2: usize,
        doc_id2: DocId,
    ) -> Result<Ordering> {
        match self {
            CrossReaderComparatorEnum::Long(l) => {
                l.compare(reader_index1, doc_id1, reader_index2, doc_id2)
            }
            CrossReaderComparatorEnum::Double(d) => {
                d.compare(reader_index1, doc_id1, reader_index2, doc_id2)
            }
        }
    }
}

trait CrossReaderComparator {
    fn compare(
        &self,
        reader_index1: usize,
        doc_id1: DocId,
        reader_index2: usize,
        doc_id2: DocId,
    ) -> Result<Ordering>;
}

struct LongCrossReaderComparator {
    docs_with_fields: Vec<BitsRef>,
    values: Vec<NumericDocValuesRef>,
    missing_value: i64,
    reverse: bool,
}

impl LongCrossReaderComparator {
    fn new(
        docs_with_fields: Vec<BitsRef>,
        values: Vec<NumericDocValuesRef>,
        missing_value: i64,
        reverse: bool,
    ) -> Self {
        LongCrossReaderComparator {
            docs_with_fields,
            values,
            missing_value,
            reverse,
        }
    }
}

impl CrossReaderComparator for LongCrossReaderComparator {
    fn compare(
        &self,
        idx1: usize,
        doc_id1: DocId,
        idx2: usize,
        doc_id2: DocId,
    ) -> Result<Ordering> {
        let value1 = if self.docs_with_fields[idx1].get(doc_id1 as usize)? {
            self.values[idx1].get(doc_id1)?
        } else {
            self.missing_value
        };
        let value2 = if self.docs_with_fields[idx2].get(doc_id2 as usize)? {
            self.values[idx2].get(doc_id2)?
        } else {
            self.missing_value
        };
        let res = value1.cmp(&value2);
        if !self.reverse {
            Ok(res.reverse())
        } else {
            Ok(res)
        }
    }
}

struct DoubleCrossReaderComparator {
    docs_with_fields: Vec<BitsRef>,
    values: Vec<NumericDocValuesRef>,
    missing_value: f64,
    reverse: bool,
}

impl DoubleCrossReaderComparator {
    fn new(
        docs_with_fields: Vec<BitsRef>,
        values: Vec<NumericDocValuesRef>,
        missing_value: f64,
        reverse: bool,
    ) -> Self {
        DoubleCrossReaderComparator {
            docs_with_fields,
            values,
            missing_value,
            reverse,
        }
    }
}

impl CrossReaderComparator for DoubleCrossReaderComparator {
    fn compare(
        &self,
        idx1: usize,
        doc_id1: DocId,
        idx2: usize,
        doc_id2: DocId,
    ) -> Result<Ordering> {
        let value1 = if self.docs_with_fields[idx1].get(doc_id1 as usize)? {
            f64::from_bits(self.values[idx1].get(doc_id1)? as u64)
        } else {
            self.missing_value
        };
        let value2 = if self.docs_with_fields[idx2].get(doc_id2 as usize)? {
            f64::from_bits(self.values[idx2].get(doc_id2)? as u64)
        } else {
            self.missing_value
        };
        let res = value1.partial_cmp(&value2).unwrap();
        if !self.reverse {
            Ok(res.reverse())
        } else {
            Ok(res)
        }
    }
}
