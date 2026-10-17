use crate::CallerIdentity;
use anyhow::Result;
/// Private backend: production has one Win32 implementation; tests inject failures.
pub(crate) trait Api {
    fn preflight(&mut self) -> Result<()>;
    fn impersonate(&mut self) -> Result<()>;
    fn capture(&mut self) -> Result<CallerIdentity>;
    fn revert(&mut self) -> bool;
    fn fatal_revert_failure(&mut self) -> !;
}
struct Revert<'a, A: Api> {
    api: &'a mut A,
    active: bool,
}
impl<A: Api> Revert<'_, A> {
    fn finish(&mut self) {
        if self.active {
            self.active = false;
            if !self.api.revert() {
                self.api.fatal_revert_failure();
            }
        }
    }
}
impl<A: Api> Drop for Revert<'_, A> {
    fn drop(&mut self) {
        self.finish();
    }
}
pub(crate) fn collect<A: Api>(api: &mut A) -> Result<CallerIdentity> {
    // Never replace or revert a preexisting impersonation token.
    api.preflight()?;
    // Arm before the call: even a failed attempt must leave a clean thread.
    let mut guard = Revert { api, active: true };
    let result = guard.api.impersonate().and_then(|()| guard.api.capture());
    guard.finish();
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::RefCell,
        panic::{AssertUnwindSafe, catch_unwind},
        rc::Rc,
    };
    struct Fake {
        at: &'static str,
        log: Rc<RefCell<Vec<&'static str>>>,
    }
    impl Api for Fake {
        fn preflight(&mut self) -> Result<()> {
            self.log.borrow_mut().push("preflight");
            anyhow::ensure!(self.at != "preflight", "denied");
            Ok(())
        }
        fn impersonate(&mut self) -> Result<()> {
            self.log.borrow_mut().push("impersonate");
            anyhow::ensure!(self.at != "impersonate", "denied");
            Ok(())
        }
        fn capture(&mut self) -> Result<CallerIdentity> {
            self.log.borrow_mut().push("capture");
            if self.at == "panic" {
                panic!("query panic")
            }
            anyhow::ensure!(self.at != "capture", "query failed");
            Ok(CallerIdentity {
                user_sid: "S-1-5-21-1-2-3-1001".into(),
                logon_sid: "S-1-5-5-4-5".into(),
                logon_attributes: 0xc0000004,
            })
        }
        fn revert(&mut self) -> bool {
            self.log.borrow_mut().push("revert");
            self.at != "revert"
        }
        fn fatal_revert_failure(&mut self) -> ! {
            self.log.borrow_mut().push("fatal");
            panic!("simulated process termination")
        }
    }
    #[test]
    fn success_restores_identity_before_returning_metadata() {
        let log = Rc::new(RefCell::new(vec![]));
        let mut f = Fake {
            at: "",
            log: log.clone(),
        };
        assert_eq!(collect(&mut f).unwrap().user_sid(), "S-1-5-21-1-2-3-1001");
        assert_eq!(
            *log.borrow(),
            ["preflight", "impersonate", "capture", "revert"]
        );
    }
    #[test]
    fn preexisting_token_or_bad_handle_never_impersonates_or_reverts() {
        let log = Rc::new(RefCell::new(vec![]));
        assert!(
            collect(&mut Fake {
                at: "preflight",
                log: log.clone()
            })
            .is_err()
        );
        assert_eq!(*log.borrow(), ["preflight"]);
    }
    #[test]
    fn failed_impersonation_has_no_capture_or_process_token_fallback() {
        let log = Rc::new(RefCell::new(vec![]));
        assert!(
            collect(&mut Fake {
                at: "impersonate",
                log: log.clone()
            })
            .is_err()
        );
        assert_eq!(*log.borrow(), ["preflight", "impersonate", "revert"]);
    }
    #[test]
    fn failed_query_restores_before_denial_without_returning_identity() {
        let log = Rc::new(RefCell::new(vec![]));
        assert!(
            collect(&mut Fake {
                at: "capture",
                log: log.clone()
            })
            .is_err()
        );
        assert_eq!(
            *log.borrow(),
            ["preflight", "impersonate", "capture", "revert"]
        );
    }
    #[test]
    fn unwind_still_restores_thread_identity_once() {
        let log = Rc::new(RefCell::new(vec![]));
        assert!(
            catch_unwind(AssertUnwindSafe(|| collect(&mut Fake {
                at: "panic",
                log: log.clone()
            })))
            .is_err()
        );
        assert_eq!(
            *log.borrow(),
            ["preflight", "impersonate", "capture", "revert"]
        );
    }
    #[test]
    fn revert_failure_terminates_instead_of_returning_success_or_error() {
        let log = Rc::new(RefCell::new(vec![]));
        assert!(
            catch_unwind(AssertUnwindSafe(|| collect(&mut Fake {
                at: "revert",
                log: log.clone()
            })))
            .is_err()
        );
        assert_eq!(
            *log.borrow(),
            ["preflight", "impersonate", "capture", "revert", "fatal"]
        );
    }
}
