use fluvio_smartstream::{smartstream, SmartOpt, Record, RecordData, Result};

#[derive(Default, SmartOpt)]
pub struct ArrayOpt {
    key: String,
}

#[smartstream(array_map, params)]
pub fn my_array_map(
    _record: &Record,
    _opt: &ArrayOpt,
) -> Result<Vec<(Option<RecordData>, RecordData)>> {
    unimplemented!()
}

fn main() {}
