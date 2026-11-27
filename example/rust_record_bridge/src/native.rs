pub struct Packet {
    pub body: Vec<u8>,
    pub labels: Vec<String>,
    pub ratio: f64,
    pub active: bool,
    pub code: Option<i64>,
}

pub fn packet(v: Packet) -> Packet {
    v
}
pub fn optional(v: Option<Packet>) -> Option<Packet> {
    v
}
pub fn result(v: Result<Packet, Vec<String>>) -> Result<Packet, Vec<String>> {
    v
}
pub fn labels(v: Vec<String>) -> Vec<String> {
    v
}
pub fn flags(v: Vec<bool>) -> Vec<bool> {
    v
}
pub fn ratios(v: Vec<f64>) -> Vec<f64> {
    v
}
pub fn maybe_labels(v: Option<Vec<String>>) -> Option<Vec<String>> {
    v
}
