//! Generate conversion from the declaration IR, without Rust enum layout assumptions.
use super::{RustBridgeSymbol, Scalar};
use crate::qualified_name::QualifiedName;
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
        self.append_rust_type(&mut result, input, None);
        result
    }
    // Append each type node once; never rebuild every prefix of a deep chain.
    fn append_rust_type(&self, output: &mut String, input: bool, lifetime: Option<&str>) {
        #[cfg(test)]
        TYPE_NODE_VISITS.set(TYPE_NODE_VISITS.get() + 1);
        match self {
            Self::Opaque(name) => {
                if input {
                    output.push('&');
                    if let Some(lifetime) = lifetime {
                        write!(output, "{lifetime} crate::").unwrap();
                    }
                    output.push_str("bridge::r#");
                } else {
                    output.push_str(if lifetime.is_some() {
                        "::std::boxed::Box<crate::bridge::r#"
                    } else {
                        "Box<bridge::r#"
                    });
                }
                output.push_str(name);
                if !input {
                    output.push('>');
                }
            }
            Self::String | Self::Bytes if input => {
                output.push('&');
                if let Some(lifetime) = lifetime {
                    write!(output, "{lifetime} ").unwrap();
                }
                output.push_str(match (*self == Self::String, lifetime.is_some()) {
                    (true, true) => "::std::primitive::str",
                    (false, true) => "[::std::primitive::u8]",
                    (true, false) => "str",
                    (false, false) => "[u8]",
                });
            }
            Self::String => output.push_str(if lifetime.is_some() {
                "::std::string::String"
            } else {
                "String"
            }),
            Self::Bytes => output.push_str(if lifetime.is_some() {
                "::std::vec::Vec<::std::primitive::u8>"
            } else {
                "Vec<u8>"
            }),
            Self::Record(name) => {
                let module = if input {
                    "record_inputs"
                } else {
                    "record_outputs"
                };
                write!(output, "crate::{module}::r#{name}").unwrap();
                if input {
                    write!(output, "<{}>", lifetime.unwrap_or("'_")).unwrap();
                }
            }
            Self::Array(t) => {
                output.push_str(if lifetime.is_some() {
                    "::std::vec::Vec<"
                } else {
                    "Vec<"
                });
                t.append_rust_type(output, input, lifetime);
                output.push('>');
            }
            Self::Option(t) => {
                output.push_str(if lifetime.is_some() {
                    "::std::option::Option<"
                } else {
                    "Option<"
                });
                t.append_rust_type(output, input, lifetime);
                output.push('>');
            }
            Self::Result(a, b) => {
                output.push_str(if lifetime.is_some() {
                    "::std::result::Result<"
                } else {
                    "Result<"
                });
                a.append_rust_type(output, input, lifetime);
                output.push(',');
                b.append_rust_type(output, input, lifetime);
                output.push('>');
            }
            _ => output.push_str(self.rust()),
        }
    }
    fn append_owned_type(&self, output: &mut String) {
        match self {
            Self::Opaque(name) => {
                write!(output, "crate::AsyncHandle<crate::bridge::r#{name}>").unwrap()
            }
            Self::Record(name) => write!(output, "crate::record_owned::r#{name}").unwrap(),
            Self::Option(t) | Self::Array(t) => {
                output.push_str(if matches!(self, Self::Option(_)) {
                    "::std::option::Option<"
                } else {
                    "::std::vec::Vec<"
                });
                t.append_owned_type(output);
                output.push('>');
            }
            Self::Result(a, b) => {
                output.push_str("::std::result::Result<");
                a.append_owned_type(output);
                output.push(',');
                b.append_owned_type(output);
                output.push('>');
            }
            _ => self.append_rust_type(output, false, Some("'static")),
        }
    }
    fn decode(&self, source: &mut String, value: &str, serial: &mut usize) -> String {
        self.decode_mode(source, value, serial, false)
    }
    fn decode_mode(
        &self,
        source: &mut String,
        value: &str,
        serial: &mut usize,
        owned: bool,
    ) -> String {
        let n = *serial;
        *serial += 1;
        let out = format!("v{n}");
        match self {
            Self::Opaque(name) if owned => writeln!(source, "let {out} = AsyncHandle::<bridge::r#{name}>::new(frame.payload({value}, false).low, std::any::TypeId::of::<opaque_tags::r#{name}>());").unwrap(),
            Self::String if owned => writeln!(source, "let {out} = frame.string({value}.low).to_owned();").unwrap(),
            Self::Bytes if owned => writeln!(source, "let {out} = frame.bytes({value}.low).to_vec();").unwrap(),
            Self::Record(name) if owned => writeln!(source, "let {out} = decode_owned_record_{name}(&frame, {value});").unwrap(),
            Self::Opaque(name) => writeln!(source, "let {out} = frame.handle::<bridge::r#{name}>(frame.payload({value}, false).low, std::any::TypeId::of::<opaque_tags::r#{name}>());").unwrap(),
            Self::I64 => writeln!(source, "let {out} = {value}.low as i64;").unwrap(),
            Self::F64 => writeln!(source, "let {out} = f64::from_bits({value}.low);").unwrap(),
            Self::Bool => writeln!(source, "let {out} = {value}.low != 0;").unwrap(),
            Self::Void => writeln!(source, "let {out} = ();").unwrap(),
            Self::String => writeln!(source, "let {out} = frame.string({value}.low);").unwrap(),
            Self::Bytes => writeln!(source, "let {out} = frame.bytes({value}.low);").unwrap(),
            Self::Record(name) => writeln!(source, "let {out} = decode_record_{name}(&frame, {value});").unwrap(),
            Self::Array(t) => {
                writeln!(source, "frame.root({value}.low);\nlet mut {out} = Vec::with_capacity(frame.array_len({value}.low));\nfor i{n} in 0..frame.array_len({value}.low) {{\nlet e{n} = frame.array_get({value}.low, i{n});").unwrap();
                let child = t.decode_mode(source, &format!("e{n}"), serial, owned);
                writeln!(source, "{out}.push({child}); }}").unwrap();
            }
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
                let child = t.decode_mode(source, &format!("p{n}"), serial, owned);
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
                let child = a.decode_mode(source, &format!("p{n}"), serial, owned);
                writeln!(source, "Ok({child}) }} else {{").unwrap();
                self.decode_payload(source, value, b, n);
                let child = b.decode_mode(source, &format!("p{n}"), serial, owned);
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
                Self::Record(name) => {
                    writeln!(source, "let {out} = encode_record_{name}(&frame, {value});").unwrap()
                }
                Self::Array(t) => {
                    writeln!(source, "let {out} = frame.array_new({value}.len(), {});\nfor (i{n}, x{n}) in {value}.into_iter().enumerate() {{", t.reference()).unwrap();
                    let child = t.encode(source, &format!("x{n}"), serial);
                    writeln!(source, "frame.array_set({out}, i{n}, {child}); }}").unwrap();
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
    let target = QualifiedName::new(&symbol.willow_function).last_segment();
    let guard = symbol.entry_guard();
    let records = &symbol.records;
    let metadata = if records.is_empty() {
        ""
    } else {
        ", records: *const *const u64"
    };
    let mutable = if symbol.close_handle { "" } else { "mut " };
    writeln!(source, "#[unsafe(no_mangle)] pub unsafe extern \"C\" fn {}(input: *const BridgeValue, output: *mut BridgeValue{metadata}) {{\nunsafe {{ {guard}({revision}); }}\nlet outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {{\nlet {mutable}frame = BridgeFrame::new();",symbol.abi_symbol).unwrap();
    for (i, record) in records.iter().enumerate() {
        let name = &record.name;
        writeln!(
            source,
            "frame.descriptors.insert({name:?}, unsafe {{ *records.add({i}) }});"
        )
        .unwrap();
    }
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
        writeln!(source, "let id = frame.payload(a0, false).low;\n{{ let _region = BridgeRegion::enter(); handles::close(id, std::any::TypeId::of::<opaque_tags::r#{name}>()).unwrap_or_else(|error| std::panic::panic_any((error, id))); }}\nunsafe {{ *output = BridgeValue::default(); }}\n}}));\nif let Err(payload) = outcome {{ bridge_panic(payload); }}\n}}").unwrap();
        return;
    }
    let mut serial = 0;
    let mut arguments = Vec::new();
    for (i, ty) in symbol.input_types.iter().enumerate() {
        arguments.push(ty.decode_mode(source, &format!("a{i}"), &mut serial, symbol.is_async));
    }
    if symbol.is_async {
        // The task constructor copies inputs now; user Rust only runs on the executor.
        for (i, _) in records.iter().enumerate() {
            writeln!(
                source,
                "let descriptor{i} = unsafe {{ *records.add({i}) }} as usize;"
            )
            .unwrap();
        }
        writeln!(source, "let future = Box::pin(async move {{ let result: {} = bridge::r#{target}({}).await; Box::new(move |frame: &mut BridgeFrame| {{", symbol.output_type.rust_type(false), arguments.join(",")).unwrap();
        // Descriptors are immutable process-lifetime metadata, not GC values.
        if !records.is_empty() {
            for (i, record) in records.iter().enumerate() {
                writeln!(
                    source,
                    "frame.descriptors.insert({:?}, descriptor{i} as *const u64);",
                    record.name
                )
                .unwrap();
            }
        }
        let encoded = symbol.output_type.encode(source, "result", &mut serial);
        writeln!(source, "{encoded} }}) as Completion }});\nlet task = spawn_future(future, {});\nunsafe {{ *output = task; }}\n}}));\nif let Err(payload) = outcome {{ bridge_panic(payload); }}\n}}", symbol.output_type.reference()).unwrap();
        return;
    }
    write!(source, "let adapter: fn(").unwrap();
    for ty in &symbol.input_types {
        write!(source, "{},", ty.rust_type(true)).unwrap();
    }
    writeln!(
        source,
        ") -> {} = bridge::r#{target};\nlet result = {{ let _region = BridgeRegion::enter(); adapter({}) }};\nframe.release_handles();",
        symbol.output_type.rust_type(false),
        arguments.join(",")
    )
    .unwrap();
    let encoded = symbol.output_type.encode(source, "result", &mut serial);
    writeln!(source,"unsafe {{ *output = {encoded}; }}\n}}));\nif let Err(payload) = outcome {{ bridge_panic(payload); }}\n}}").unwrap();
}

// Adapter structs have ordinary Rust layout. Only generated field conversions
// cross the ABI, so a Rust repr attribute or layout agreement is unnecessary.
pub(super) fn record_structs(source: &mut String, symbols: &[RustBridgeSymbol]) {
    let mut seen = std::collections::HashMap::new();
    let mut records = Vec::new();
    for record in symbols.iter().flat_map(|s| &s.records) {
        if let Some(previous) = seen.insert(&record.name, record) {
            if previous != record {
                writeln!(
                    source,
                    "compile_error!({:?});",
                    format!(
                        "rust_bridge_signature_mismatch: conflicting record declarations for {}",
                        record.name
                    )
                )
                .unwrap();
            }
        } else {
            records.push(record);
        }
    }
    source.push_str("#[allow(dead_code)] pub mod records {\n");
    for record in &records {
        let super::Record { name, fields } = record;
        write!(source, "pub struct r#{name}").unwrap();
        if !fields.is_empty() {
            source.push('<');
            for i in 0..fields.len() {
                write!(source, "T{i},").unwrap();
            }
            source.push('>');
        }
        source.push_str(" { ");
        for (i, (field, _)) in fields.iter().enumerate() {
            write!(source, "pub r#{field}: T{i},").unwrap();
        }
        source.push_str(" }\n");
    }
    source.push_str("}\n");
    for input in [true, false] {
        let module = if input {
            "record_inputs"
        } else {
            "record_outputs"
        };
        writeln!(source, "#[allow(dead_code)] pub mod {module} {{").unwrap();
        for record in &records {
            let super::Record { name, fields } = record;
            write!(
                source,
                "pub type r#{name}{} = crate::records::r#{name}",
                if input { "<'a>" } else { "" }
            )
            .unwrap();
            if !fields.is_empty() {
                source.push('<');
                for (_, ty) in fields {
                    ty.append_rust_type(source, input, Some("'a"));
                    source.push(',');
                }
                source.push('>');
            }
            source.push_str(";\n");
        }
        source.push_str("}\n");
    }
    if symbols.iter().any(|s| s.is_async) {
        source.push_str("#[allow(dead_code)] pub mod record_owned {\n");
        for record in &records {
            write!(
                source,
                "pub type r#{} = crate::records::r#{}",
                record.name, record.name
            )
            .unwrap();
            if !record.fields.is_empty() {
                source.push('<');
                for (_, ty) in &record.fields {
                    ty.append_owned_type(source);
                    source.push(',');
                }
                source.push('>');
            }
            source.push_str(";\n");
        }
        source.push_str("}\n");
        for record in &records {
            let name = &record.name;
            writeln!(source, "#[allow(non_snake_case, dead_code)] fn decode_owned_record_{name}(frame: &BridgeFrame, value: BridgeValue) -> record_owned::r#{name} {{ frame.root(value.low);").unwrap();
            let mut offset = 1;
            let mut serial = 0;
            let mut values = Vec::new();
            for (field, ty) in &record.fields {
                writeln!(
                    source,
                    "let p{offset} = frame.record_get(value.low, {offset}, {});",
                    ty.pair()
                )
                .unwrap();
                let value = ty.decode_mode(source, &format!("p{offset}"), &mut serial, true);
                values.push(format!("r#{field}: {value}"));
                offset += if ty.pair() { 2 } else { 1 };
            }
            writeln!(source, "records::r#{name} {{ {} }} }}", values.join(",")).unwrap();
        }
    }
    for record in &records {
        let super::Record { name, fields } = record;
        writeln!(source, "#[allow(non_snake_case, dead_code)] fn decode_record_{name}(frame: &BridgeFrame, value: BridgeValue) -> record_inputs::r#{name}<'_> {{ frame.root(value.low);").unwrap();
        let mut serial = 0;
        let mut offset = 1usize;
        let mut values = Vec::new();
        for (field, ty) in fields {
            writeln!(
                source,
                "let p{offset} = frame.record_get(value.low, {offset}, {});",
                ty.pair()
            )
            .unwrap();
            let child = ty.decode(source, &format!("p{offset}"), &mut serial);
            values.push(format!("r#{field}: {child}"));
            offset += if ty.pair() { 2 } else { 1 };
        }
        writeln!(source, "records::r#{name} {{ {} }} }}", values.join(",")).unwrap();
        let words = offset;
        let mut bitmap = vec![0u64; words.div_ceil(64) + 1];
        bitmap[0] = (bitmap.len() - 1) as u64;
        offset = 1;
        for (_, ty) in fields {
            if ty.reference() {
                bitmap[1 + offset / 64] |= 1 << (offset % 64);
            }
            offset += if ty.pair() { 2 } else { 1 };
        }
        let digest = willow_abi::gc_bitmap_fingerprint(&bitmap[1..]);
        writeln!(source, "#[allow(non_snake_case, dead_code)] fn encode_record_{name}(frame: &BridgeFrame, value: record_outputs::r#{name}) -> BridgeValue {{ let object = frame.record_new(frame.descriptors[{name:?}], {words}, &{bitmap:?}, {digest});").unwrap();
        offset = 1;
        for (field, ty) in fields {
            let child = ty.encode(source, &format!("value.r#{field}"), &mut serial);
            writeln!(
                source,
                "frame.record_set(object, {offset}, {child}, {}, {});",
                ty.pair(),
                ty.reference()
            )
            .unwrap();
            offset += if ty.pair() { 2 } else { 1 };
        }
        source.push_str("object }\n");
    }
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
