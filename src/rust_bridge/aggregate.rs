//! Generate conversion from the declaration IR, without Rust enum layout assumptions.
use super::{RustBridgeSymbol, Scalar};
use std::fmt::Write;
#[cfg(test)]
thread_local! { static TYPE_NODE_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

impl Scalar {
    pub fn aggregate(&self) -> bool {
        !matches!(self, Self::I64 | Self::F64 | Self::Bool | Self::Void)
    }
    pub fn pair(&self) -> bool {
        match self {
            Self::Option(t) => !t.aggregate(),
            Self::Result(a, b) => !a.aggregate() && !b.aggregate(),
            _ => false,
        }
    }
    pub fn reference(&self) -> bool {
        self.aggregate() && !self.pair()
    }
    fn niche(&self) -> bool {
        matches!(self, Self::Option(t) if t.reference() && !matches!(**t, Self::Option(_)))
    }
    fn rust_type(&self, input: bool) -> String {
        let mut result = String::new();
        self.append_rust_type(&mut result, input);
        result
    }
    // Append each type node once; never rebuild every prefix of a deep chain.
    fn append_rust_type(&self, output: &mut String, input: bool) {
        #[cfg(test)]
        TYPE_NODE_VISITS.set(TYPE_NODE_VISITS.get() + 1);
        match self {
            Self::Opaque(name) => {
                if input {
                    output.push_str("&bridge::r#");
                } else {
                    output.push_str("Box<bridge::r#");
                }
                output.push_str(name);
                if !input {
                    output.push('>');
                }
            }
            Self::String => output.push_str(if input { "&str" } else { "String" }),
            Self::Bytes => output.push_str(if input { "&[u8]" } else { "Vec<u8>" }),
            Self::Option(t) => {
                output.push_str("Option<");
                t.append_rust_type(output, input);
                output.push('>');
            }
            Self::Result(a, b) => {
                output.push_str("Result<");
                a.append_rust_type(output, input);
                output.push(',');
                b.append_rust_type(output, input);
                output.push('>');
            }
            _ => output.push_str(self.rust()),
        }
    }
    fn decode(&self, source: &mut String, value: &str, serial: &mut usize) -> String {
        let n = *serial;
        *serial += 1;
        let out = format!("v{n}");
        match self {
            Self::Opaque(name) => writeln!(source, "let {out} = frame.handle::<bridge::r#{name}>(frame.payload({value}, false).low, std::any::TypeId::of::<opaque_tags::r#{name}>());").unwrap(),
            Self::I64 => writeln!(source, "let {out} = {value}.low as i64;").unwrap(),
            Self::F64 => writeln!(source, "let {out} = f64::from_bits({value}.low);").unwrap(),
            Self::Bool => writeln!(source, "let {out} = {value}.low != 0;").unwrap(),
            Self::Void => writeln!(source, "let {out} = ();").unwrap(),
            Self::String => writeln!(source, "let {out} = frame.string({value}.low);").unwrap(),
            Self::Bytes => writeln!(source, "let {out} = frame.bytes({value}.low);").unwrap(),
            Self::Option(t) => {
                let tag = if self.niche() {
                    format!("u64::from({value}.low == 0)")
                } else if self.pair() {
                    format!("{value}.low")
                } else {
                    format!("frame.tag({value})")
                };
                writeln!(source, "let {out} = if {tag} == 0 {{").unwrap();
                self.decode_payload(source, value, t, n);
                let child = t.decode(source, &format!("p{n}"), serial);
                writeln!(source, "Some({child}) }} else {{ None }};").unwrap();
            }
            Self::Result(a, b) => {
                let tag = if self.pair() {
                    format!("{value}.low")
                } else {
                    format!("frame.tag({value})")
                };
                writeln!(source, "let {out} = if {tag} == 0 {{").unwrap();
                self.decode_payload(source, value, a, n);
                let child = a.decode(source, &format!("p{n}"), serial);
                writeln!(source, "Ok({child}) }} else {{").unwrap();
                self.decode_payload(source, value, b, n);
                let child = b.decode(source, &format!("p{n}"), serial);
                writeln!(source, "Err({child}) }};").unwrap();
            }
        }
        out
    }
    fn decode_payload(&self, source: &mut String, value: &str, child: &Scalar, n: usize) {
        if self.niche() {
            writeln!(source, "let p{n} = {value};").unwrap();
        } else if self.pair() {
            writeln!(
                source,
                "let p{n} = BridgeValue {{ low: {value}.high, high: 0 }};"
            )
            .unwrap();
        } else {
            writeln!(
                source,
                "let p{n} = frame.payload({value}, {});",
                child.pair()
            )
            .unwrap();
        }
    }
    fn encode(&self, source: &mut String, value: &str, serial: &mut usize) -> String {
        let n = *serial;
        *serial += 1;
        let out = format!("r{n}");
        let bits = match self {
            Self::I64 => Some(format!("{value} as u64")),
            Self::F64 => Some(format!("{value}.to_bits()")),
            Self::Bool => Some(format!("u64::from({value})")),
            Self::Void => Some("0".into()),
            _ => None,
        };
        if let Some(bits) = bits {
            writeln!(
                source,
                "let {out} = BridgeValue {{ low: {bits}, high: 0 }};"
            )
            .unwrap();
        } else {
            match self {
                Self::Opaque(name) => {
                    writeln!(source, "let id{n} = handles::insert({value}, std::any::TypeId::of::<opaque_tags::r#{name}>());\nlet {out} = frame.enum_value(0, BridgeValue {{ low: id{n}, high: 0 }}, false, false);").unwrap();
                }
                Self::String | Self::Bytes => {
                    let method = if *self == Self::String {
                        "output_string"
                    } else {
                        "output_bytes"
                    };
                    writeln!(source, "let {out} = frame.{method}(&{value});").unwrap();
                }
                Self::Option(t) => {
                    writeln!(source, "let {out} = match {value} {{ Some(x{n}) => {{").unwrap();
                    let child = t.encode(source, &format!("x{n}"), serial);
                    let packed = self.pack(&child, t, 0);
                    writeln!(
                        source,
                        "{packed} }}, None => {} }};",
                        if self.niche() {
                            "BridgeValue::default()".into()
                        } else {
                            self.pack("BridgeValue::default()", &Scalar::Void, 1)
                        }
                    )
                    .unwrap();
                }
                Self::Result(a, b) => {
                    writeln!(source, "let {out} = match {value} {{ Ok(x{n}) => {{").unwrap();
                    let child = a.encode(source, &format!("x{n}"), serial);
                    writeln!(source, "{} }}, Err(x{n}) => {{", self.pack(&child, a, 0)).unwrap();
                    let child = b.encode(source, &format!("x{n}"), serial);
                    writeln!(source, "{} }} }};", self.pack(&child, b, 1)).unwrap();
                }
                _ => unreachable!(),
            }
        }
        out
    }
    fn pack(&self, value: &str, child: &Scalar, tag: u64) -> String {
        if self.niche() {
            value.into()
        } else if self.pair() {
            format!("BridgeValue {{ low: {tag}, high: ({value}).low }}")
        } else {
            format!(
                "frame.enum_value({tag}, {value}, {}, {})",
                child.pair(),
                child.reference()
            )
        }
    }
}

pub(super) fn aggregate_wrapper(source: &mut String, symbol: &RustBridgeSymbol, revision: u32) {
    let target = symbol.willow_function.rsplit("::").next().unwrap();
    let mutable = if symbol.close_handle { "" } else { "mut " };
    writeln!(source, "#[unsafe(no_mangle)] pub unsafe extern \"C\" fn {}(input: *const BridgeValue, output: *mut BridgeValue) {{\nunsafe {{ willow_rust_bridge_enter({revision}); }}\nlet outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {{\nlet {mutable}frame = BridgeFrame::new();",symbol.abi_symbol).unwrap();
    for (i, ty) in symbol.input_types.iter().enumerate() {
        writeln!(source, "let a{i} = unsafe {{ *input.add({i}) }};").unwrap();
        if ty.reference() {
            writeln!(source, "frame.root(a{i}.low);").unwrap();
        }
    }
    if symbol.close_handle {
        let Scalar::Opaque(name) = &symbol.input_types[0] else {
            unreachable!()
        };
        writeln!(source, "let id = frame.payload(a0, false).low;\nhandles::close(id, std::any::TypeId::of::<opaque_tags::r#{name}>()).unwrap_or_else(|error| std::panic::panic_any((error, id)));\nunsafe {{ *output = BridgeValue::default(); }}\n}}));\nif let Err(payload) = outcome {{ bridge_panic(payload); }}\n}}").unwrap();
        return;
    }
    let mut serial = 0;
    let mut arguments = Vec::new();
    for (i, ty) in symbol.input_types.iter().enumerate() {
        arguments.push(ty.decode(source, &format!("a{i}"), &mut serial));
    }
    write!(source, "let adapter: fn(").unwrap();
    for ty in &symbol.input_types {
        write!(source, "{},", ty.rust_type(true)).unwrap();
    }
    writeln!(
        source,
        ") -> {} = bridge::r#{target};\nlet result = adapter({});\nframe.release_handles();",
        symbol.output_type.rust_type(false),
        arguments.join(",")
    )
    .unwrap();
    let encoded = symbol.output_type.encode(source, "result", &mut serial);
    writeln!(source,"unsafe {{ *output = {encoded}; }}\n}}));\nif let Err(payload) = outcome {{ bridge_panic(payload); }}\n}}").unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn type_generation_visits_each_node_once_in_deep_and_wide_types() {
        for depth in [1, 8, 32, 128] {
            let mut ty = Scalar::I64;
            for _ in 0..depth {
                ty = Scalar::Option(Box::new(ty));
            }
            TYPE_NODE_VISITS.set(0);
            let text = ty.rust_type(true);
            assert_eq!(TYPE_NODE_VISITS.get(), depth + 1);
            assert_eq!(text.len(), 8 * depth + 3);
            eprintln!(
                "type_depth={depth} visits={} bytes={}",
                TYPE_NODE_VISITS.get(),
                text.len()
            );
        }
        for depth in [1, 3, 5, 7] {
            let mut ty = Scalar::I64;
            for _ in 0..depth {
                ty = Scalar::Result(Box::new(ty.clone()), Box::new(ty));
            }
            TYPE_NODE_VISITS.set(0);
            let text = ty.rust_type(true);
            let leaves = 1usize << depth;
            assert_eq!(TYPE_NODE_VISITS.get(), 2 * leaves - 1);
            assert_eq!(text.len(), 3 * leaves + 9 * (leaves - 1));
            eprintln!(
                "type_leaves={leaves} visits={} bytes={}",
                TYPE_NODE_VISITS.get(),
                text.len()
            );
        }
    }
}
