// Copyright lowRISC contributors.
// Licensed under the Apache License, Version 2.0, see LICENSE for details.
// SPDX-License-Identifier: Apache-2.0

//! A `manticore` "server" and "client" for a PA-RoT.
//!
//! This module provides structures for serving responses to a host making
//! requests to a PA-RoT.

use crate::cert;
use crate::crypto::csrng;
use crate::crypto::hash;
use crate::crypto::hash::EngineExt as _;
use crate::crypto::sig;
use crate::hardware;
use crate::mem::Arena;
use crate::mem::ArenaExt as _;
use crate::net;
use crate::net::CerberusHeader;
use crate::net::SpdmHeader;
use crate::protocol::cerberus;
use crate::protocol::spdm;
use crate::protocol::Req;
use crate::protocol::Resp;
use crate::server::Error;
use crate::session::Session;

use crate::server::handler::prelude::*;

/// Options struct for initializing a [`PaRot`].
pub struct Options<'a> {
    /// A handle to the "hardware identity" of the device.
    pub identity: &'a dyn hardware::Identity,
    /// A handle for looking up reset-related information for the current
    /// device.
    pub reset: &'a dyn hardware::Reset,

    /// A handle to a hashing engine.
    pub hasher: &'a mut dyn hash::Engine,
    /// A handle to a signature verification engine,
    pub ciphers: &'a mut dyn sig::Ciphers,
    /// A random number generator for creating nonces and ephemeral keys.
    pub csrng: &'a mut dyn csrng::Csrng,
    /// The trust chain to use for the challenge.
    pub trust_chain: &'a mut dyn cert::TrustChain,

    /// The session manager.
    pub session: &'a mut dyn Session,

    /// The value of PMR0.
    ///
    /// Eventually this should be replaced with a general "PMRs"
    /// trait.
    pub pmr0: &'a [u8],

    /// This device's silicon identifier.
    pub device_id: cerberus::device_id::DeviceIdentifier,
    /// Integration-provided description of the device's networking
    /// capabilities.
    pub networking: cerberus::capabilities::Networking,
    /// Integration-provided "acceptable timeout" lengths.
    pub timeouts: cerberus::capabilities::Timeouts,
}

/// A PA-RoT, or "Platform Root of Trust", server.
///
/// This type implements the request -> response "business logic" of the
/// host <-> PA-RoT interaction. That is, it accepts input and output buffers,
/// and from those, parses incoming requests and processes them into responses.
pub struct PaRot<'a> {
    opts: Options<'a>,
    ok_count: u16,
    err_count: u16,

    /// State from the last `GetDigests`, which records whether the
    /// following `Challenge` will be used to initiate a key exchange.
    /// This prevents the `Challenge` from clobbering session state if
    /// key exchange won't happen.
    key_exchange: Option<cerberus::get_digests::KeyExchangeAlgo>,

    /// The most recent certificate slot used for an ECDH-seeding
    /// `Challenge`. This records which certificate's key needs to sign
    /// the ECDH keypair in the key exchange.
    ///
    /// Note that this is *only* changed when the most recent `GetDigests`
    /// indicated a forthcoming key exchange.
    current_cert_slot: Option<u8>,
}

impl<'a> PaRot<'a> {
    /// Create a new `PaRot` with the given `Options`.
    pub fn new(opts: Options<'a>) -> Self {
        Self {
            opts,
            ok_count: 0,
            err_count: 0,
            key_exchange: None,
            current_cert_slot: None,
        }
    }

    /// Process a single incoming request.
    pub fn process_request<'req>(
        &mut self,
        host_port: &mut dyn net::host::HostPort<'req, CerberusHeader>,
        arena: &'req dyn Arena,
    ) -> Result<(), Error<CerberusHeader>> {
        // Style note: when defining a new handler, if it is more than a
        // handful of lines long, define it out-of-line instead.
        let result = Handler::<&mut Self, CerberusHeader>::new()
            .handle::<cerberus::FirmwareVersion, _>(|ctx| {
                ctx.server.handle_fw_version(&ctx.req)
            })
            .handle::<cerberus::DeviceCapabilities, _>(|ctx| {
                ctx.server.handle_capabilities(&ctx.req)
            })
            .handle::<cerberus::DeviceId, _>(|ctx| {
                Ok(Resp::<cerberus::DeviceId> {
                    id: ctx.server.opts.device_id,
                })
            })
            .handle::<cerberus::DeviceInfo, _>(|ctx| {
                Ok(Resp::<cerberus::DeviceInfo> {
                    info: ctx.server.opts.identity.unique_device_identity(),
                })
            })
            .handle::<cerberus::GetDigests, _>(|ctx| {
                ctx.server.handle_digests(ctx.arena, &ctx.req)
            })
            .handle::<cerberus::GetCert, _>(|ctx| {
                ctx.server.handle_cert(&ctx.req)
            })
            .handle_buffered::<cerberus::Challenge, _>(|ctx| {
                ctx.server
                    .handle_challenge(ctx.arena, &ctx.req, ctx.req_buf)
            })
            .handle::<cerberus::KeyExchange, _>(|ctx| {
                ctx.server.handle_key_xchg(ctx.arena, &ctx.req)
            })
            .handle::<cerberus::ResetCounter, _>(|ctx| {
                use cerberus::reset_counter::ResetType;
                // NOTE: Currently, we only handle "local resets" for port 0,
                // the "self" port.
                if ctx.req.reset_type != ResetType::Local
                    || ctx.req.port_id != 0
                {
                    return Err(cerberus::Error::OutOfRange);
                }

                Ok(Resp::<cerberus::ResetCounter> {
                    count: ctx.server.opts.reset.resets_since_power_on() as u16,
                })
            })
            .handle::<cerberus::DeviceUptime, _>(|ctx| {
                // NOTE: Currently, we only handle port 0, the "self" port.
                if ctx.req.port_id != 0 {
                    return Err(cerberus::Error::OutOfRange);
                }

                Ok(Resp::<cerberus::DeviceUptime> {
                    uptime: ctx.server.opts.reset.uptime(),
                })
            })
            .handle::<cerberus::RequestCounter, _>(|ctx| {
                Ok(Resp::<cerberus::RequestCounter> {
                    ok_count: ctx.server.ok_count,
                    err_count: ctx.server.err_count,
                })
            })
            .run(self, host_port, arena);

        match result {
            Ok(_) => self.ok_count += 1,
            Err(_) => self.err_count += 1,
        }
        result
    }

    fn handle_fw_version(
        &mut self,
        req: &Req<cerberus::FirmwareVersion>,
    ) -> Result<Resp<cerberus::FirmwareVersion>, cerberus::Error> {
        if req.index == 0 {
            return Ok(Resp::<cerberus::FirmwareVersion> {
                version: self.opts.identity.firmware_version(),
            });
        }

        let version = self
            .opts
            .identity
            .vendor_firmware_version(req.index)
            .ok_or(cerberus::Error::OutOfRange)?;
        Ok(Resp::<cerberus::FirmwareVersion> { version })
    }

    fn handle_capabilities(
        &mut self,
        req: &Req<cerberus::DeviceCapabilities>,
    ) -> Result<Resp<cerberus::DeviceCapabilities>, cerberus::Error> {
        use cerberus::capabilities::*;
        use enumflags2::BitFlags;
        let mut crypto = req.capabilities.crypto;

        self.opts.ciphers.negotiate(&mut crypto);
        crypto.has_aes = false;
        crypto.aes_strength = BitFlags::<AesKeyStrength>::empty();

        let capabilities = Capabilities {
            networking: self.opts.networking,
            security: BitFlags::<Security>::empty(),

            has_pfm_support: false,
            has_policy_support: false,
            has_firmware_protection: false,

            crypto,
        };

        Ok(Resp::<cerberus::DeviceCapabilities> {
            capabilities,
            timeouts: self.opts.timeouts,
        })
    }

    fn handle_digests<'req>(
        &mut self,
        arena: &'req dyn Arena,
        req: &Req<cerberus::GetDigests>,
    ) -> Result<Resp<'req, cerberus::GetDigests>, cerberus::Error> {
        let digests_len = self
            .opts
            .trust_chain
            .chain_len(req.slot)
            .ok_or(cerberus::Error::UnknownChain)?
            .get();
        let digests = arena
            .alloc_slice::<[u8; hash::Algo::Sha256.bytes()]>(digests_len)?;
        for (i, digest) in digests.iter_mut().enumerate() {
            let cert = self
                .opts
                .trust_chain
                .cert(req.slot, i)
                .ok_or(cerberus::Error::UnknownChain)?;
            self.opts.hasher.contiguous_hash(
                hash::Algo::Sha256,
                cert.raw(),
                digest,
            )?;
        }

        self.key_exchange = Some(req.key_exchange);
        Ok(Resp::<cerberus::GetDigests> { digests })
    }

    fn handle_cert(
        &mut self,
        req: &Req<cerberus::GetCert>,
    ) -> Result<Resp<cerberus::GetCert>, cerberus::Error> {
        let cert = self
            .opts
            .trust_chain
            .cert(req.slot, req.cert_number as usize)
            .ok_or(cerberus::Error::UnknownChain)?;

        let start = cert.raw().len().min(req.offset as usize);
        let end = cert
            .raw()
            .len()
            .min((req.len as usize).saturating_add(start));
        Ok(Resp::<cerberus::GetCert> {
            slot: req.slot,
            cert_number: req.cert_number,
            data: &cert.raw()[start..end],
        })
    }

    fn handle_challenge<'req>(
        &'req mut self,
        arena: &'req dyn Arena,
        req: &Req<cerberus::Challenge>,
        req_buf: &[u8],
    ) -> Result<Resp<'req, cerberus::Challenge>, cerberus::Error> {
        let signer = self
            .opts
            .trust_chain
            .signer(req.slot)
            .ok_or(cerberus::Error::UnknownChain)?;
        let nonce = arena.alloc::<[u8; 32]>()?;
        self.opts.csrng.fill(nonce)?;

        let tbs = cerberus::challenge::ChallengeResponseTbs {
            slot: req.slot,
            slot_mask: 0, // Currently unspecified?
            protocol_range: (0, 0),
            nonce,
            pmr0_components: 0,
            pmr0: self.opts.pmr0,
        };

        let signature = arena.alloc_slice::<u8>(signer.sig_bytes())?;
        let sig_len = tbs.as_iovec_with(|[a, b, c, d]| {
            signer.sign(&[req_buf, a, b, c, d], signature)
        })?;
        let signature = &signature[..sig_len];

        if let Some(cerberus::get_digests::KeyExchangeAlgo::Ecdh) =
            self.key_exchange
        {
            self.opts.session.create_session(req.nonce, tbs.nonce)?;
            self.current_cert_slot = Some(tbs.slot);
        }

        Ok(Resp::<cerberus::Challenge> { tbs, signature })
    }

    fn handle_key_xchg<'req>(
        &mut self,
        arena: &'req dyn Arena,
        req: &Req<cerberus::KeyExchange>,
    ) -> Result<Resp<'req, cerberus::KeyExchange>, cerberus::Error> {
        use cerberus::key_exchange::*;
        match req {
            Req::<KeyExchange>::SessionKey {
                hmac_algorithm,
                pk_req,
            } => {
                let slot = self
                    .current_cert_slot
                    .ok_or(cerberus::Error::OutOfRange)?;
                let signer = self
                    .opts
                    .trust_chain
                    .signer(slot)
                    .ok_or(cerberus::Error::UnknownChain)?;

                let pk_resp =
                    arena.alloc_slice(self.opts.session.ephemeral_bytes())?;
                let key_len = self.opts.session.begin_ecdh(pk_resp)?;
                let pk_resp = &pk_resp[..key_len];
                self.opts.session.finish_ecdh(*hmac_algorithm, pk_req)?;

                let signature = arena.alloc_slice(signer.sig_bytes())?;
                signer.sign(&[pk_req, pk_resp], signature)?;

                let chain_len = self
                    .opts
                    .trust_chain
                    .chain_len(slot)
                    .ok_or(cerberus::Error::OutOfRange)?
                    .get();
                let alias_cert = self
                    .opts
                    .trust_chain
                    .cert(slot, chain_len - 1)
                    .ok_or(cerberus::Error::OutOfRange)?;

                let alias_cert_hmac = hmac_algorithm.alloc(arena)?;
                let (_, hmac_key) = self
                    .opts
                    .session
                    .hmac_key()
                    .ok_or(cerberus::Error::Internal)?;
                self.opts.hasher.contiguous_hmac(
                    *hmac_algorithm,
                    hmac_key,
                    alias_cert.raw(),
                    alias_cert_hmac,
                )?;

                Ok(Resp::<KeyExchange>::SessionKey {
                    pk_resp,
                    signature,
                    alias_cert_hmac,
                })
            }
            _ => Err(cerberus::Error::Internal),
        }
    }

    /// Process a single incoming SPDM request.
    pub fn process_spdm_request<'req>(
        &mut self,
        host_port: &mut dyn net::host::HostPort<'req, SpdmHeader>,
        arena: &'req dyn Arena,
    ) -> Result<(), Error<SpdmHeader>> {
        // Style note: when defining a new handler, if it is more than a
        // handful of lines long, define it out-of-line instead.
        let result = Handler::<&mut Self, SpdmHeader>::new()
            .handle::<spdm::GetVersion, _>(|_| {
                Ok(Resp::<spdm::GetVersion> {
                    versions: &[spdm::ExtendedVersion::MANTICORE],
                })
            })
            .handle::<spdm::GetCaps, _>(|ctx| {
                Ok(Resp::<spdm::GetCaps> {
                    crypto_timeout: ctx.server.opts.timeouts.crypto,
                    caps: spdm::get_caps::Caps::manticore(),
                    max_packet_size: ctx.server.opts.networking.max_packet_size
                        as u32,
                    max_message_size: ctx
                        .server
                        .opts
                        .networking
                        .max_message_size
                        as u32,
                })
            })
            .run(self, host_port, arena);

        match result {
            Ok(_) => self.ok_count += 1,
            Err(_) => self.err_count += 1,
        }
        result
    }
}
