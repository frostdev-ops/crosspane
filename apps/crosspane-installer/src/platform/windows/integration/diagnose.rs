//! Observation-only report projection; it has no Apply, lock, cleanup, task or launch port.
use super::domains::{DEFERRED, Snapshot, State};
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Fact {
    pub check: &'static str,
    pub value: &'static str,
    pub blocking: bool,
}
fn name(state: State) -> &'static str {
    match state {
        State::Healthy => "healthy",
        State::Missing => "missing",
        State::Mismatch => "mismatch",
        State::Disabled => "user_disabled",
        State::Unavailable => "unavailable",
        State::Unknown => "unknown",
    }
}
pub(crate) fn facts(snapshot: &Snapshot) -> Vec<Fact> {
    let mut out = vec![
        Fact {
            check: "Limited user and fixed-root authority",
            value: if snapshot.supported {
                "admitted"
            } else {
                "unknown"
            },
            blocking: !snapshot.supported,
        },
        Fact {
            check: "embedded approved inventory",
            value: if snapshot.inventory {
                "available"
            } else {
                "unavailable"
            },
            blocking: !snapshot.inventory,
        },
        Fact {
            check: "fixed payload",
            value: name(snapshot.payload),
            blocking: snapshot.payload != State::Healthy,
        },
        Fact {
            check: "exact task",
            value: name(snapshot.task),
            blocking: snapshot.task != State::Healthy,
        },
        Fact {
            check: "authenticated agent health",
            value: name(snapshot.agent),
            blocking: snapshot.agent != State::Healthy,
        },
        Fact {
            check: "operation publication",
            value: if snapshot.unsettled {
                "unknown"
            } else {
                "settled"
            },
            blocking: snapshot.unsettled,
        },
        Fact {
            check: "terminal artifacts",
            value: if snapshot.terminal_history {
                "retained"
            } else {
                "none_observed"
            },
            blocking: false,
        },
    ];
    out.extend(DEFERRED.iter().map(|check| Fact {
        check,
        value: "deferred",
        blocking: false,
    }));
    out
}
#[cfg(all(windows, not(test)))]
pub(crate) fn run(payload: Option<&std::path::Path>, report: &mut crate::diagnose::Report) {
    let _ = payload; // Diagnose never opens the supplied readers or constructs an operation domain.
    match super::domains::native::read_observation() {
        Ok((snapshot, _)) => {
            for fact in facts(&snapshot) {
                if fact.blocking {
                    report.issue(
                        "windows",
                        fact.check,
                        crate::diagnose::Class::S,
                        fact.value,
                        true,
                    );
                } else {
                    report.record::<_, &str>(
                        "windows",
                        fact.check,
                        crate::diagnose::Class::S,
                        Ok(fact.value),
                        false,
                    );
                }
            }
        }
        Err(_) => report.unavailable_target(),
    }
}
