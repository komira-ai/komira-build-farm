//! The erase verbs (M4.2, S5.2, S8): the relay for an operator-signed request,
//! `grant-admin`, `bring-forward`, and the tick that runs scheduled erases.
//!
//! - A request is checked once, when it arrives: signature and touch, the serial it
//!   signs (which must be the one the server asked about), inventory, `not-after`
//!   (passed, or more than an hour ahead, is refused), and its nonce. The nonce is spent
//!   by any request whose signature verified, even one the caps then refuse, so the
//!   server cannot keep a refused request and replay it once the caps allow.
//! - `erase-now` runs at once within the caps (one erase outstanding fleet-wide, the
//!   daily cap, the Mac floor) or is refused; it is never held.
//! - `privileged-lease <lease>` is held for `grant-admin` of that serial and lease, and
//!   discarded with an alert 24 hours after acceptance if no grant uses it; from that
//!   moment `grant-admin` refuses it, whether or not the tick has discarded it yet.
//! - The daily cap counts erases sent in the last 24 hours plus erases scheduled and not
//!   yet sent. `erase-now` and `grant-admin` are refused unless that sum is below the
//!   cap; a scheduled erase is sent whatever the cap and then counts as sent. The sum
//!   therefore never exceeds the cap, so no 24 hours see more erases than the cap.
//! - Once a request's signature has verified, every alert about it, refusals included,
//!   names its signer and its signed purpose.
//! - `grant-admin` uses a held request, reserves an erase within the daily cap and the
//!   floor, schedules the erase at grant time plus the longest lease, and returns a
//!   grant signed with the gate's key. The scheduled erase runs whatever the request's
//!   `not-after`; if another erase is outstanding then, it waits, and is never dropped.
//! - `bring-forward` only moves an erase the gate scheduled with a grant to now.

use serde::{Deserialize, Serialize};

use super::{Gate, Refusal};
use crate::backend::MdmBackend;
use crate::clock::{DAY, HOUR, format_rfc3339};
use crate::grant::Grant;
use crate::journal::Event;
use crate::request::{Purpose, parse};
use crate::state::{Held, Outstanding, Scheduled, State};

/// What the server relays: the operator's message and its armored signature, unchanged.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedRequest {
    pub message: String,
    pub signature: String,
}

/// What an accepted request led to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "outcome")]
pub enum EraseOutcome {
    /// The erase was sent.
    Erased,
    /// The request is held for `grant-admin` of this lease.
    Held { lease: String },
}

/// What `grant-admin` returns.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Granted {
    #[serde(flatten)]
    pub grant: Grant,
    /// When the gate will erase the Mac.
    pub erase_at: String,
}

/// What `bring-forward` did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "outcome")]
pub enum BroughtForward {
    /// The erase was sent now.
    Erased,
    /// Another erase is outstanding; this one runs as soon as it clears.
    Waiting { behind: String },
}

impl<B: MdmBackend> Gate<B> {
    /// `erase <signed request>`, as the server relays it for `serial`.
    ///
    /// # Errors
    /// Any check refuses the request, or the MDM fails. Every refusal alerts.
    pub async fn erase(
        &self,
        serial: &str,
        request: &SignedRequest,
    ) -> Result<EraseOutcome, Refusal> {
        let now = self.clock.now();
        let mut signed_as = None;
        let result = self.erase_inner(serial, request, now, &mut signed_as).await;
        if let Err(refusal) = &result {
            // Before the signature verifies, nothing in the request is the operator's.
            let detail = match signed_as {
                Some(who) => format!("{refusal}; request {who}"),
                None => refusal.to_string(),
            };
            self.record(&Event::new(now, "erase", serial, "refused", detail, true))?;
        }
        result
    }

    /// `signed_as` receives the signed purpose, signer and reason once the signature
    /// has verified and the text parsed, for the alerts.
    async fn erase_inner(
        &self,
        asked: &str,
        request: &SignedRequest,
        now: i64,
        signed_as: &mut Option<String>,
    ) -> Result<EraseOutcome, Refusal> {
        let text = crate::trusted::read_text(&self.files.allowed_signers, self.files.owner)
            .map_err(Refusal::Internal)?;
        let signers = crate::signers::AllowedSigners::parse(&text)
            .map_err(|e| Refusal::Internal(e.to_string()))?;
        let signer = signers.verify(
            request.message.as_bytes(),
            &request.signature,
            now,
            self.policy.require_user_verified,
        )?;
        let signed = parse(&request.message)?;
        let purpose = match &signed.purpose {
            Purpose::EraseNow => "erase-now".to_owned(),
            Purpose::PrivilegedLease(lease) => format!("privileged-lease {lease}"),
        };
        let who = format!(
            "{purpose} signed by {} ({}): {}",
            signer.principals, signer.algorithm, signed.reason
        );
        *signed_as = Some(who.clone());
        // The serial comes from the signed message; the server's only says which Mac it
        // meant, and a difference is refused.
        if signed.serial != asked {
            return Err(Refusal::SerialMismatch {
                signed: signed.serial,
                asked: asked.to_owned(),
            });
        }
        let mac = self.mac(&signed.serial)?;
        if signed.not_after < now {
            return Err(Refusal::Expired);
        }
        if signed.not_after > now + HOUR {
            return Err(Refusal::TooFarAhead);
        }
        let mut state = self.state.lock().await;
        if state.nonces.contains_key(&signed.nonce) {
            return Err(Refusal::Replay);
        }
        state.nonces.insert(signed.nonce.clone(), signed.not_after);
        self.save(&state)?;
        match signed.purpose {
            Purpose::EraseNow => {
                self.check_erase_now(&state, &mac.serial, now)?;
                self.send_erase(&mut state, &mac.serial, now).await?;
                self.record(&Event::new(now, "erase", &mac.serial, "erased", who, true))?;
                Ok(EraseOutcome::Erased)
            }
            Purpose::PrivilegedLease(lease) => {
                if state
                    .held
                    .iter()
                    .any(|h| h.serial == mac.serial && h.lease == lease)
                {
                    return Err(Refusal::AlreadyHeld {
                        serial: mac.serial.clone(),
                        lease,
                    });
                }
                state.held.push(Held {
                    serial: mac.serial.clone(),
                    lease: lease.clone(),
                    signer: signer.principals,
                    nonce: signed.nonce,
                    accepted_at: now,
                    not_after: signed.not_after,
                });
                self.save(&state)?;
                self.record(&Event::new(now, "erase", &mac.serial, "held", who, true))?;
                Ok(EraseOutcome::Held { lease })
            }
        }
    }

    /// The caps on an erase sent now.
    fn check_erase_now(&self, state: &State, serial: &str, now: i64) -> Result<(), Refusal> {
        if let Some(o) = &state.outstanding_erase {
            return Err(Refusal::Busy(format!(
                "an erase of {} is outstanding",
                o.serial
            )));
        }
        self.check_cap(state, now)?;
        self.check_floor(state, serial)
    }

    /// Erases sent in the last 24 hours plus erases scheduled and not yet sent must stay
    /// below the cap for one more to be sent or reserved.
    fn check_cap(&self, state: &State, now: i64) -> Result<(), Refusal> {
        if Self::erases_last_day(state, now) + state.scheduled.len() >= self.policy.daily_erase_cap
        {
            return Err(Refusal::DailyCap(self.policy.daily_erase_cap));
        }
        Ok(())
    }

    /// Marks the erase outstanding and saves, then sends it; undoes the marks if the MDM
    /// refuses. The state is saved first so a crash after sending cannot lose the
    /// outstanding erase. Every erase sent counts toward the daily cap; a scheduled one
    /// counted as scheduled until now.
    async fn send_erase(&self, state: &mut State, serial: &str, now: i64) -> Result<(), Refusal> {
        let device = self.mac(serial)?.device();
        let before = state.clone();
        state.outstanding_erase = Some(Outstanding {
            serial: serial.to_owned(),
            started_at: now,
        });
        state.erased.insert(serial.to_owned(), now);
        state.erase_times.push(now);
        self.save(state)?;
        if let Err(e) = self.backend.erase(device).await {
            *state = before;
            self.save(state)?;
            return Err(e.into());
        }
        Ok(())
    }

    /// `grant-admin <serial> <lease>` (S5.2).
    ///
    /// # Errors
    /// No held request names this Mac and lease, the daily cap or the floor refuses it.
    pub async fn grant_admin(&self, serial: &str, lease: &str) -> Result<Granted, Refusal> {
        let now = self.clock.now();
        let result = self.grant_inner(serial, lease, now).await;
        let (outcome, detail) = match &result {
            Ok(g) => ("granted", format!("lease {lease}; erase at {}", g.erase_at)),
            Err(r) => ("refused", format!("lease {lease}: {r}")),
        };
        self.record(&Event::new(
            now,
            "grant-admin",
            serial,
            outcome,
            detail,
            true,
        ))?;
        result
    }

    async fn grant_inner(&self, serial: &str, lease: &str, now: i64) -> Result<Granted, Refusal> {
        let mac = self.mac(serial)?;
        let mut state = self.state.lock().await;
        let index = state
            .held
            .iter()
            // A request 24 hours old is refused even before the tick discards it.
            .position(|h| h.serial == mac.serial && h.lease == lease && now < h.accepted_at + DAY)
            .ok_or_else(|| Refusal::NoHeldRequest {
                serial: serial.to_owned(),
                lease: lease.to_owned(),
            })?;
        self.check_cap(&state, now)?;
        self.check_floor(&state, serial)?;
        let held = state.held.remove(index);
        let due = now + self.policy.max_lease_secs;
        state.scheduled.push(Scheduled {
            serial: held.serial,
            lease: held.lease,
            signer: held.signer,
            granted_at: now,
            due,
            not_after: held.not_after,
        });
        self.save(&state)?;
        Ok(Granted {
            grant: self.grant_key.sign(serial, lease, now),
            erase_at: format_rfc3339(due),
        })
    }

    /// `bring-forward <serial> <lease>`: runs the erase the gate scheduled with that
    /// lease's grant now, or as soon as an outstanding erase clears.
    ///
    /// # Errors
    /// The gate issued no grant for that Mac and lease, or the MDM fails.
    pub async fn bring_forward(
        &self,
        serial: &str,
        lease: &str,
    ) -> Result<BroughtForward, Refusal> {
        let now = self.clock.now();
        let mac = self.mac(serial)?;
        let mut state = self.state.lock().await;
        let Some(scheduled) = state
            .scheduled
            .iter_mut()
            .find(|s| s.serial == mac.serial && s.lease == lease)
        else {
            let refusal = Refusal::NoGrant {
                serial: serial.to_owned(),
                lease: lease.to_owned(),
            };
            self.record(&Event::new(
                now,
                "bring-forward",
                serial,
                "refused",
                refusal.to_string(),
                true,
            ))?;
            return Err(refusal);
        };
        scheduled.due = scheduled.due.min(now);
        self.save(&state)?;
        self.record(&Event::new(
            now,
            "bring-forward",
            serial,
            "accepted",
            format!("lease {lease}"),
            true,
        ))?;
        self.run_due(&mut state, now).await?;
        Ok(match &state.outstanding_erase {
            Some(o) if o.serial == serial && o.started_at == now => BroughtForward::Erased,
            other => BroughtForward::Waiting {
                behind: other
                    .as_ref()
                    .map_or_else(String::new, |o| o.serial.clone()),
            },
        })
    }

    /// Runs the earliest due scheduled erase, if no erase is outstanding.
    async fn run_due(&self, state: &mut State, now: i64) -> Result<(), Refusal> {
        if state.outstanding_erase.is_some() {
            return Ok(());
        }
        let Some(index) = (0..state.scheduled.len())
            .filter(|&i| state.scheduled[i].due <= now)
            .min_by_key(|&i| state.scheduled[i].due)
        else {
            return Ok(());
        };
        let job = state.scheduled.remove(index);
        if let Err(e) = self.send_erase(state, &job.serial, now).await {
            let detail = format!("lease {}: {e}; will retry", job.lease);
            state.scheduled.push(job.clone());
            self.save(state)?;
            self.record(&Event::new(
                now,
                "scheduled-erase",
                &job.serial,
                "failed",
                detail,
                true,
            ))?;
            return Ok(());
        }
        let detail = format!(
            "lease {} granted {} signed by {}",
            job.lease,
            format_rfc3339(job.granted_at),
            job.signer
        );
        self.record(&Event::new(
            now,
            "scheduled-erase",
            &job.serial,
            "erased",
            detail,
            true,
        ))
    }

    /// The periodic work: forget expired nonces and old cap entries, discard held
    /// requests 24 hours old, clear an outstanding erase once its Mac reports again (or
    /// after 24 hours), and run due scheduled erases.
    ///
    /// # Errors
    /// The state or the audit log cannot be written.
    pub async fn tick(&self) -> Result<(), Refusal> {
        let now = self.clock.now();
        let mut state = self.state.lock().await;
        state.nonces.retain(|_, &mut not_after| not_after >= now);
        state.erase_times.retain(|&t| t > now - DAY);
        let (stale, kept): (Vec<_>, Vec<_>) = state
            .held
            .drain(..)
            .partition(|h| h.accepted_at + DAY <= now);
        state.held = kept;
        for h in stale {
            let detail = format!(
                "privileged-lease {} signed by {}: no grant within 24 h",
                h.lease, h.signer
            );
            self.record(&Event::new(
                now,
                "erase",
                &h.serial,
                "discarded",
                detail,
                true,
            ))?;
        }
        self.clear_reported(&mut state).await;
        if state
            .outstanding_erase
            .as_ref()
            .is_some_and(|o| now >= o.started_at + DAY)
        {
            state.outstanding_erase = None;
        }
        self.save(&state)?;
        self.run_due(&mut state, now).await
    }

    /// Forgets erased Macs that reported after their erase (re-enrolled).
    ///
    /// The signal is any status value the MDM recorded later than the second the erase
    /// was sent. **[A]** that a Mac reports nothing between the erase being queued and
    /// it running: NanoHUB's API exposes no enrollment or check-in time, and a report
    /// in that gap would clear the outstanding erase early, letting the next erase pass
    /// the one-at-a-time rule (the daily cap and the floor still hold).
    async fn clear_reported(&self, state: &mut State) {
        let erased: Vec<(String, i64)> =
            state.erased.iter().map(|(s, t)| (s.clone(), *t)).collect();
        for (serial, erased_at) in erased {
            let seen = match self.inventory.0.get(&serial) {
                None => true,
                Some(mac) => match self.backend.status(mac.device()).await {
                    Ok(status) => status.last_seen.is_some_and(|t| t > erased_at),
                    Err(e) => {
                        tracing::warn!(%serial, "status after erase: {e}");
                        false
                    }
                },
            };
            if seen {
                state.erased.remove(&serial);
                if state
                    .outstanding_erase
                    .as_ref()
                    .is_some_and(|o| o.serial == serial)
                {
                    state.outstanding_erase = None;
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "gate_erase_tests.rs"]
mod tests;
