use std::fmt;

use serde::*;

use crate::{grid_fmt::GridFmt, Primitive, Uiua, UiuaResult, Value};

#[derive(Clone, Serialize, Deserialize)]
pub enum DynArr {
    InfiniteRange(u64),
}

impl DynArr {
    pub fn materialize(self, env: &Uiua) -> UiuaResult<Value> {
        match self {
            DynArr::InfiniteRange(_) => Err(env.error("Cannot materialize infinite range")),
        }
    }
    pub fn type_name(&self) -> &'static str {
        match self {
            DynArr::InfiniteRange(_) => "infinite range",
        }
    }
}

impl fmt::Debug for DynArr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use Primitive::*;
        match self {
            DynArr::InfiniteRange(0) => write!(f, "{Range}{Infinity}"),
            DynArr::InfiniteRange(start) => write!(f, "{Drop}{}{Range}{Infinity}", start),
        }
    }
}

impl GridFmt for DynArr {}
