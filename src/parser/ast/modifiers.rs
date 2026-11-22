bitflags::bitflags! {
    /// Source modifiers shared by the declaration parser and AST.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Modifiers: u8 {
        const PUBLIC = 1;
        const PROTECTED = 1 << 1;
        const ASYNC = 1 << 2;
        const OPEN = 1 << 3;
        const OVERRIDE = 1 << 4;
        /// A class-level method or property, without an instance receiver.
        const STATIC = 1 << 5;
        /// Mutability of a static property.
        const MUT = 1 << 6;
    }
}

// Keep the historical flat serde fields, including their order and defaults.
// Borrow payloads when writing and move them when reading. In particular, a
// declaration's parameters and body never pass through an intermediate Value.
macro_rules! declaration {
    (
        $(#[$doc:meta])* pub struct $name:ident {
            $($(#[doc = $head_doc:literal])* $(#[serde($($head_attr:tt)*)])? pub $head:ident: $head_ty:ty,)*
            modifiers { $($flag:ident => $bit:ident),+ $(,)? }
            $($(#[doc = $tail_doc:literal])* $(#[serde($($tail_attr:tt)*)])? pub $tail:ident: $tail_ty:ty,)*
        }
    ) => {
        $(#[$doc])*
        #[derive(Clone)]
        pub struct $name {
            $($(#[doc = $head_doc])* pub $head: $head_ty,)*
            pub modifiers: Modifiers,
            $($(#[doc = $tail_doc])* pub $tail: $tail_ty,)*
        }

        impl $name {
            $(
                #[inline]
                pub fn $flag(&self) -> bool {
                    self.modifiers.contains(Modifiers::$bit)
                }
            )+
        }

        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct(stringify!($name))
                    $(.field(stringify!($head), &self.$head))*
                    $(.field(stringify!($flag), &self.$flag()))+
                    $(.field(stringify!($tail), &self.$tail))*
                    .finish()
            }
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                #[derive(serde::Serialize)]
                struct $name<'a> {
                    $($(#[serde($($head_attr)*)])? $head: &'a $head_ty,)*
                    $($flag: bool,)+
                    $($(#[serde($($tail_attr)*)])? $tail: &'a $tail_ty,)*
                }
                $name {
                    $($head: &self.$head,)* $($flag: self.$flag(),)+ $($tail: &self.$tail,)*
                }.serialize(serializer)
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                #[derive(serde::Deserialize)]
                struct $name {
                    $($(#[serde($($head_attr)*)])? $head: $head_ty,)*
                    $($flag: bool,)+
                    $($(#[serde($($tail_attr)*)])? $tail: $tail_ty,)*
                }
                let wire = $name::deserialize(deserializer)?;
                let mut modifiers = Modifiers::empty();
                $(modifiers.set(Modifiers::$bit, wire.$flag);)+
                Ok(Self { $($head: wire.$head,)* modifiers, $($tail: wire.$tail,)* })
            }
        }
    };
}

pub(super) use declaration;
