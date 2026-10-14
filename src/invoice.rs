use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvoiceState {
    Draft,
    Open,
    Paid,
    Void,
    Uncollectible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvoiceEvent {
    Issue,
    PaymentSucceeded,
    Void,
    MarkUncollectible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidTransition {
    pub current: InvoiceState,
    pub event: InvoiceEvent,
}

impl InvoiceState {
    pub fn transition(self, event: InvoiceEvent) -> Result<Self, InvalidTransition> {
        match (self, event) {
            (Self::Draft, InvoiceEvent::Issue) => Ok(Self::Open),

            (Self::Draft, InvoiceEvent::Void) => Ok(Self::Void),

            (Self::Open, InvoiceEvent::PaymentSucceeded) => Ok(Self::Paid),

            (Self::Open, InvoiceEvent::Void) => Ok(Self::Void),

            (Self::Open, InvoiceEvent::MarkUncollectible) => Ok(Self::Uncollectible),

            _ => Err(InvalidTransition {
                current: self,
                event,
            }),
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Paid | Self::Void | Self::Uncollectible)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Open => "open",
            Self::Paid => "paid",
            Self::Void => "void",
            Self::Uncollectible => "uncollectible",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issuing_draft_invoice_opens_it() {
        let result = InvoiceState::Draft.transition(InvoiceEvent::Issue);

        assert_eq!(result, Ok(InvoiceState::Open));
    }

    #[test]
    fn successful_payment_marks_open_invoice_paid() {
        let result = InvoiceState::Open.transition(InvoiceEvent::PaymentSucceeded);

        assert_eq!(result, Ok(InvoiceState::Paid));
    }

    #[test]
    fn failed_payment_does_not_change_invoice_state() {
        // There is intentionally no PaymentFailed transition event.
        // A failed attempt leaves the invoice Open.
        let state = InvoiceState::Open;

        assert_eq!(state, InvoiceState::Open);
        assert!(!state.is_terminal());
    }

    #[test]
    fn paid_invoice_cannot_be_voided() {
        let result = InvoiceState::Paid.transition(InvoiceEvent::Void);

        assert_eq!(
            result,
            Err(InvalidTransition {
                current: InvoiceState::Paid,
                event: InvoiceEvent::Void,
            })
        );
    }

    #[test]
    fn void_invoice_cannot_be_paid() {
        let result = InvoiceState::Void.transition(InvoiceEvent::PaymentSucceeded);

        assert!(result.is_err());
    }

    #[test]
    fn draft_invoice_can_be_voided() {
        let result = InvoiceState::Draft.transition(InvoiceEvent::Void);

        assert_eq!(result, Ok(InvoiceState::Void));
    }

    #[test]
    fn open_invoice_can_be_marked_uncollectible() {
        let result = InvoiceState::Open.transition(InvoiceEvent::MarkUncollectible);

        assert_eq!(result, Ok(InvoiceState::Uncollectible));
    }

    #[test]
    fn terminal_states_are_terminal() {
        assert!(InvoiceState::Paid.is_terminal());
        assert!(InvoiceState::Void.is_terminal());
        assert!(InvoiceState::Uncollectible.is_terminal());

        assert!(!InvoiceState::Draft.is_terminal());
        assert!(!InvoiceState::Open.is_terminal());
    }

    #[test]
    fn state_names_match_database_values() {
        assert_eq!(InvoiceState::Draft.as_str(), "draft");
        assert_eq!(InvoiceState::Open.as_str(), "open");
        assert_eq!(InvoiceState::Paid.as_str(), "paid");
        assert_eq!(InvoiceState::Void.as_str(), "void");
        assert_eq!(InvoiceState::Uncollectible.as_str(), "uncollectible");
    }
}
