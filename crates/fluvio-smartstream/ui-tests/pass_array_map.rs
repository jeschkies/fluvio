use fluvio_smartstream::{smartstream, Record, RecordData, Result};

#[smartstream(array_map)]
pub fn my_array_map(_record: &Record) -> Result<Vec<(Option<RecordData>, RecordData)>> {
    unimplemented!()
}

fn main() {}
