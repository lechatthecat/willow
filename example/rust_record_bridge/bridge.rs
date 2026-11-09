// Generated structs keep input borrows separate from owned outputs.
use crate::records::Packet;
type Input<'a> = Packet<&'a [u8], Vec<&'a str>, f64, bool, Option<i64>>;
type Output = Packet<Vec<u8>, Vec<String>, f64, bool, Option<i64>>;
pub fn packet(v: Input<'_>) -> Output {
    Packet { body: v.body.to_vec(), labels: labels(v.labels), ratio: v.ratio, active: v.active, code: v.code }
}
pub fn optional(v: Option<Input<'_>>) -> Option<Output> { v.map(packet) }
pub fn result(v: Result<Input<'_>, Vec<&str>>) -> Result<Output, Vec<String>> { v.map(packet).map_err(labels) }
pub fn labels(v: Vec<&str>) -> Vec<String> { v.into_iter().map(str::to_owned).collect() }
pub fn flags(v: Vec<bool>) -> Vec<bool> { v }
pub fn ratios(v: Vec<f64>) -> Vec<f64> { v }
pub fn maybe_labels(v: Option<Vec<&str>>) -> Option<Vec<String>> { v.map(labels) }
