//! The environment switches that take an execution path out of use, each
//! read in one place: the operator that honours a switch and the startup
//! report that names it ask the same function.

/// `PINTAIL_DISABLE_SETTLED_MEMO` makes every settled aggregate execute
/// instead of replaying its remembered answer. Read on every call.
pub(super) fn settled_memo_disabled() -> bool {
    std::env::var_os("PINTAIL_DISABLE_SETTLED_MEMO").is_some()
}

/// `PINTAIL_DISABLE_ARGUMENT_PROJECTION` keeps computed aggregate
/// arguments on the general path. Read on every call.
pub(super) fn argument_projection_disabled() -> bool {
    std::env::var_os("PINTAIL_DISABLE_ARGUMENT_PROJECTION").is_some()
}

/// `PINTAIL_DISABLE_PACKED_GROUP` keeps a composite `GROUP BY` off the
/// packed-key fold. Read on every call.
pub(super) fn packed_group_disabled() -> bool {
    std::env::var_os("PINTAIL_DISABLE_PACKED_GROUP").is_some()
}

/// `PINTAIL_DISABLE_FUSED_FOLD` keeps an aggregate over a scan pulling
/// batches a round at a time instead of folding each slice on the thread
/// that decoded it. Read once.
pub(super) fn fused_fold_disabled() -> bool {
    static DISABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DISABLED.get_or_init(|| std::env::var_os("PINTAIL_DISABLE_FUSED_FOLD").is_some())
}

/// One execution path and the switch that decides it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PathSwitch {
    /// What the path is called in the startup report.
    pub name: &'static str,
    /// The environment variable that turns it off.
    pub variable: &'static str,
    /// Whether the path is in use in this process.
    pub enabled: bool,
}

/// The executor's switchable paths as this process runs them.
#[must_use]
pub fn path_switches() -> [PathSwitch; 5] {
    [
        PathSwitch {
            name: "settled_memo",
            variable: "PINTAIL_DISABLE_SETTLED_MEMO",
            enabled: !settled_memo_disabled(),
        },
        PathSwitch {
            name: "packed_group",
            variable: "PINTAIL_DISABLE_PACKED_GROUP",
            enabled: !packed_group_disabled(),
        },
        PathSwitch {
            name: "grouped_fold",
            variable: "PINTAIL_DISABLE_GROUPED_FOLD",
            enabled: !super::aggregate::grouped_fold_disabled(),
        },
        PathSwitch {
            name: "argument_projection",
            variable: "PINTAIL_DISABLE_ARGUMENT_PROJECTION",
            enabled: !argument_projection_disabled(),
        },
        PathSwitch {
            name: "fused_fold",
            variable: "PINTAIL_DISABLE_FUSED_FOLD",
            enabled: !fused_fold_disabled(),
        },
    ]
}
