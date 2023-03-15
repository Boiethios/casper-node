#![allow(clippy::bool_comparison)]

pub mod logger;
pub mod rules;
pub mod stack_height;

mod ext;
mod gas;
mod optimizer;
mod symbols;

pub use ext::{
    externalize, externalize_mem, shrink_unknown_stack, underscore_funcs, ununderscore_funcs,
};
pub use gas::inject_gas_counter;
pub use optimizer::{optimize, Error as OptimizerError};

pub struct TargetSymbols {
    pub create: &'static str,
    pub call: &'static str,
    pub ret: &'static str,
}

pub enum TargetRuntime {
    Substrate(TargetSymbols),
    PWasm(TargetSymbols),
}

impl TargetRuntime {
    pub fn substrate() -> TargetRuntime {
        TargetRuntime::Substrate(TargetSymbols {
            create: "deploy",
            call: "call",
            ret: "ext_return",
        })
    }

    pub fn pwasm() -> TargetRuntime {
        TargetRuntime::PWasm(TargetSymbols {
            create: "deploy",
            call: "call",
            ret: "ret",
        })
    }

    pub fn symbols(&self) -> &TargetSymbols {
        match self {
            TargetRuntime::Substrate(s) => s,
            TargetRuntime::PWasm(s) => s,
        }
    }
}
