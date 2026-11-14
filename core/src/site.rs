//! Describing a place in the game's code the mod needs, without saying where it is.

/// One way of finding a site. A [`Site`] lists several, from the most exact to the most forgiving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// A byte signature (`48 8B ?? C3`) that must match exactly once in `.text`; the site is its first byte.
    Sig(&'static str),
    /// The function that references a unique string of the data sections (an assert message, a file name in the
    /// code, a format). Survives recompilation much better than raw bytes, because the string rarely changes.
    /// The site is the start of that function.
    StringXref(&'static str),
    /// The `index`-th distinct function (in order of first call) that the function identified by the string `anchor`
    /// calls with `call rel32`. For helpers that have no string of their own but are called by one that has.
    CalleeOfAnchor(&'static str, usize),
}

impl Strategy {
    pub fn method(&self) -> &'static str {
        match self {
            Strategy::Sig(_) => "signature",
            Strategy::StringXref(_) => "string reference",
            Strategy::CalleeOfAnchor(..) => "callee of anchored function",
        }
    }
}

/// How much damage a wrong hook here could do. Safe mode turns features off from the highest risk down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Risk {
    /// Reads only, or wraps a leaf call.
    Low,
    /// Hooks a function and changes what it returns or receives.
    Medium,
    /// Writes into game structures or hooks deep in the engine.
    High,
}

/// Where inside its function the located address is expected to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    /// The first instruction of a function: validated against what precedes it.
    FunctionStart,
    /// Anywhere (a patch point inside a function).
    Anywhere,
}

#[derive(Debug, Clone, Copy)]
pub struct Site {
    pub name: &'static str,
    /// Tried in order; the first one that finds a valid result wins.
    pub strategies: &'static [Strategy],
    pub entry: Entry,
    pub risk: Risk,
}

impl Site {
    pub const fn new(name: &'static str, strategies: &'static [Strategy], entry: Entry, risk: Risk) -> Self {
        Self {
            name,
            strategies,
            entry,
            risk,
        }
    }
}
