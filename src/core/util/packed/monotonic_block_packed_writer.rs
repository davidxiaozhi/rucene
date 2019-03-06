use core::store::DataOutput;
use core::util::packed::monotonic_block_packed_reader::MonotonicBlockPackedReader;
use core::util::packed::packed_misc::unsigned_bits_required;
use core::util::packed::{AbstractBlockPackedWriter, BaseBlockPackedWriter};
use error::*;

pub struct MonotonicBlockPackedWriter {
    base_writer: BaseBlockPackedWriter,
}

impl MonotonicBlockPackedWriter {
    pub fn new(block_size: usize) -> MonotonicBlockPackedWriter {
        MonotonicBlockPackedWriter {
            base_writer: BaseBlockPackedWriter::new(block_size),
        }
    }
}

impl AbstractBlockPackedWriter for MonotonicBlockPackedWriter {
    fn add(&mut self, l: i64, out: &mut DataOutput) -> Result<()> {
        debug_assert!(l >= 0);
        self.base_writer.check_not_finished()?;
        if self.base_writer.off == self.base_writer.values.len() {
            self.flush(out)?;
        }
        self.base_writer.values[self.base_writer.off] = l;
        self.base_writer.off += 1;
        self.base_writer.ord += 1;
        Ok(())
    }

    fn finish(&mut self, out: &mut DataOutput) -> Result<()> {
        self.base_writer.check_not_finished()?;
        if self.base_writer.off > 0 {
            self.flush(out)?;
        }
        self.base_writer.finished = true;
        Ok(())
    }

    fn flush(&mut self, out: &mut DataOutput) -> Result<()> {
        debug_assert!(self.base_writer.off > 0);
        let avg = if self.base_writer.off == 1 {
            0f32
        } else {
            (self.base_writer.values[self.base_writer.off - 1] - self.base_writer.values[0]) as f32
                / (self.base_writer.off - 1) as f32
        };
        let mut min = self.base_writer.values[0];
        // adjust min so that all deltas will be positive
        for i in 1..self.base_writer.off {
            let actual = self.base_writer.values[i];
            let expected = MonotonicBlockPackedReader::expected(min, avg, i as i32);
            if expected > actual {
                min -= expected - actual;
            }
        }

        let mut max_delta = 0i64;
        for i in 0..self.base_writer.off {
            self.base_writer.values[i] -= MonotonicBlockPackedReader::expected(min, avg, i as i32);
            max_delta = max_delta.max(self.base_writer.values[i]);
        }

        out.write_zlong(min)?;
        out.write_int(avg.to_bits() as i32)?;
        if max_delta == 0 {
            out.write_vint(0)?;
        } else {
            let bits_required = unsigned_bits_required(max_delta);
            out.write_vint(bits_required)?;
            self.base_writer.write_values(bits_required, out)?;
        }

        self.base_writer.off = 0;
        Ok(())
    }

    fn reset(&mut self) {
        self.base_writer.reset();
    }
}
