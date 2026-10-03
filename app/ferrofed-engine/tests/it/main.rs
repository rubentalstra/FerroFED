// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! Integration tests: the crate boundaries, the pinned releases, node
//! dispatch against a mock node (§11.1, N16, N28, N33), the fan-out under both
//! completion strategies against mock nodes (§11.3 to §11.5, N37, N38), the
//! Tier order and `LIMIT` over the node answers (§11.6.1, N39),
//! single-node forwarding (§7a.3, N22, N31, N33), and the EHR create of the
//! admission check (§12b.1), and the caller's identity on every request to a
//! node (§13.1, N24, CP-16).

mod architecture;
mod conveyance;
mod conveyed;
mod definition;
mod dispatch;
mod ehr;
mod fanout;
mod forward;
mod gate;
mod masking;
mod onward;
mod pins;
mod probe;
