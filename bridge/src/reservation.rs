use anyhow::{anyhow, Result};

pub const RESERVATION_EMPTY: u8 = 0;
pub const RESERVATION_RESERVED: u8 = 1;
pub const RESERVATION_FINALIZED: u8 = 2;
pub const RESERVATION_CANCELLED: u8 = 3;
pub const RESERVATION_REDEEMED: u8 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReservationAction {
    SubmitReserve,
    ResumeApproved,
    RecordCompleted,
}

fn require_approved(approved: Option<Result<bool>>) -> Result<()> {
    let approved = approved
        .ok_or_else(|| anyhow!("reservation approval was not read"))?
        .map_err(|err| anyhow!("failed to read reservation approval: {err}"))?;
    if approved {
        Ok(())
    } else {
        Err(anyhow!(
            "Zama rejected the reservation; no lock or mint will be attempted"
        ))
    }
}

pub fn reservation_resume(
    status: Result<u8>,
    approved: Option<Result<bool>>,
) -> Result<ReservationAction> {
    let status = status.map_err(|err| anyhow!("failed to read reservation status: {err}"))?;
    match status {
        RESERVATION_EMPTY => Ok(ReservationAction::SubmitReserve),
        RESERVATION_RESERVED | RESERVATION_FINALIZED => {
            require_approved(approved)?;
            Ok(ReservationAction::ResumeApproved)
        }
        RESERVATION_CANCELLED => Err(anyhow!(
            "reservation was cancelled; no lock or mint will be attempted"
        )),
        RESERVATION_REDEEMED => {
            require_approved(approved)?;
            Ok(ReservationAction::RecordCompleted)
        }
        other => Err(anyhow!("unexpected zama reservation status {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejected_reservation_does_not_resume() {
        let err = reservation_resume(Ok(RESERVATION_RESERVED), Some(Ok(false))).unwrap_err();
        assert!(
            err.to_string().contains("rejected"),
            "rejected reservation must stop the workflow: {err}"
        );
    }

    #[test]
    fn cancelled_reservation_does_not_resume() {
        assert!(reservation_resume(Ok(RESERVATION_CANCELLED), None).is_err());
    }

    #[test]
    fn completed_reservation_does_not_start_another_operation() {
        assert!(reservation_resume(Ok(RESERVATION_REDEEMED), None).is_err());
        assert!(reservation_resume(Ok(RESERVATION_REDEEMED), Some(Ok(false))).is_err());
    }

    #[test]
    fn failed_status_read_is_not_approval() {
        assert!(reservation_resume(Err(anyhow!("rpc down")), Some(Ok(true))).is_err());
    }

    #[test]
    fn failed_approval_read_is_not_approval() {
        assert!(reservation_resume(
            Ok(RESERVATION_RESERVED),
            Some(Err(anyhow!("decrypt failed")))
        )
        .is_err());
        assert!(reservation_resume(
            Ok(RESERVATION_FINALIZED),
            Some(Err(anyhow!("decrypt failed")))
        )
        .is_err());
    }

    #[test]
    fn rejected_then_finalized_does_not_resume() {
        let err = reservation_resume(Ok(RESERVATION_FINALIZED), Some(Ok(false))).unwrap_err();
        assert!(
            err.to_string().contains("rejected"),
            "finalized rejected reservation must stop the workflow: {err}"
        );
        assert!(reservation_resume(Ok(RESERVATION_FINALIZED), None).is_err());
    }

    #[test]
    fn approved_reservation_resumes_the_same_operation() {
        assert_eq!(
            reservation_resume(Ok(RESERVATION_RESERVED), Some(Ok(true))).unwrap(),
            ReservationAction::ResumeApproved
        );
        assert_eq!(
            reservation_resume(Ok(RESERVATION_FINALIZED), Some(Ok(true))).unwrap(),
            ReservationAction::ResumeApproved
        );
        assert_eq!(
            reservation_resume(Ok(RESERVATION_EMPTY), None).unwrap(),
            ReservationAction::SubmitReserve
        );
    }

    #[test]
    fn redeemed_approved_operation_is_recorded_not_restarted() {
        assert_eq!(
            reservation_resume(Ok(RESERVATION_REDEEMED), Some(Ok(true))).unwrap(),
            ReservationAction::RecordCompleted
        );
        assert!(reservation_resume(Ok(RESERVATION_REDEEMED), Some(Ok(false))).is_err());
        assert!(reservation_resume(
            Ok(RESERVATION_REDEEMED),
            Some(Err(anyhow!("decrypt failed")))
        )
        .is_err());
    }
}
