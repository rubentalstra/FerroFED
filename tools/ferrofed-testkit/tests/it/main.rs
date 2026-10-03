// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The testkit's own suite: the proxy, the seed builder, the harness PIX
//! Manager and the harness care services directory offline against stub
//! nodes and the IHE clients, the unreachable base, and the container harness
//! behind the `FERROFED_E2E` gate.

mod e2e;
mod mcsd;
mod pix;
mod proxy;
mod seed;
mod unreachable;
mod xcpd;
