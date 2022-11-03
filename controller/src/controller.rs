// (c) 2020-2022 ZeroTier, Inc. -- currently propritery pending actual release and licensing. See LICENSE.md.

use std::collections::HashMap;
use std::error::Error;
use std::sync::{Arc, Mutex, RwLock, Weak};

use tokio::time::{Duration, Instant};

use zerotier_network_hypervisor::protocol;
use zerotier_network_hypervisor::protocol::{PacketBuffer, DEFAULT_MULTICAST_LIMIT, ZEROTIER_VIRTUAL_NETWORK_DEFAULT_MTU};
use zerotier_network_hypervisor::vl1::*;
use zerotier_network_hypervisor::vl2;
use zerotier_network_hypervisor::vl2::multicastauthority::MulticastAuthority;
use zerotier_network_hypervisor::vl2::networkconfig::*;
use zerotier_network_hypervisor::vl2::v1::Revocation;
use zerotier_network_hypervisor::vl2::NetworkId;
use zerotier_utils::blob::Blob;
use zerotier_utils::buffer::OutOfBoundsError;
use zerotier_utils::dictionary::Dictionary;
use zerotier_utils::error::InvalidParameterError;
use zerotier_utils::reaper::Reaper;
use zerotier_utils::tokio;
use zerotier_utils::{ms_monotonic, ms_since_epoch};
use zerotier_vl1_service::VL1Service;

use crate::database::*;
use crate::model::{AuthorizationResult, Member, RequestLogItem, CREDENTIAL_WINDOW_SIZE_DEFAULT};

// A netconf per-query task timeout, just a sanity limit.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// ZeroTier VL2 network controller packet handler, answers VL2 netconf queries.
pub struct Controller {
    self_ref: Weak<Self>,
    service: RwLock<Weak<VL1Service<dyn Database, Self, Self>>>,
    reaper: Reaper,
    runtime: tokio::runtime::Handle,
    database: Arc<dyn Database>,
    local_identity: Identity,

    /// Handler for MULTICAST_LIKE and MULTICAST_GATHER messages.
    multicast_authority: MulticastAuthority,

    /// Async tasks that should be killed when the controller is dropped.
    daemons: Mutex<Vec<tokio::task::JoinHandle<()>>>, // drop() aborts these

    /// Recently authorized network members and when that authorization expires (in monotonic ticks).
    /// Note that this is not and should not be used for real authentication, just for locking up multicast info.
    recently_authorized: RwLock<HashMap<[u8; Identity::FINGERPRINT_SIZE], HashMap<NetworkId, i64>>>,
}

impl Controller {
    /// Start an inner protocol handler answer ZeroTier VL2 network controller queries.
    ///
    /// The start() method must be called once the service this will run within is also created.
    pub async fn new(database: Arc<dyn Database>, runtime: tokio::runtime::Handle) -> Result<Arc<Self>, Box<dyn Error>> {
        if let Some(local_identity) = database.load_node_identity() {
            assert!(local_identity.secret.is_some());
            Ok(Arc::new_cyclic(|r| Self {
                self_ref: r.clone(),
                service: RwLock::new(Weak::default()),
                reaper: Reaper::new(&runtime),
                runtime,
                database: database.clone(),
                local_identity,
                multicast_authority: MulticastAuthority::new(),
                daemons: Mutex::new(Vec::with_capacity(2)),
                recently_authorized: RwLock::new(HashMap::new()),
            }))
        } else {
            Err(Box::new(InvalidParameterError(
                "local controller's identity not readable by database",
            )))
        }
    }

    /// Set the service and HostSystem implementation for this controller and start daemons.
    ///
    /// This must be called once the service that uses this handler is up or the controller
    /// won't actually do anything. The reference the handler holds is weak to prevent
    /// a circular reference, so if the VL1Service is dropped this must be called again to
    /// tell the controller handler about a new instance.
    pub async fn start(&self, service: &Arc<VL1Service<dyn Database, Self, Self>>) {
        *self.service.write().unwrap() = Arc::downgrade(service);

        // Create database change listener.
        if let Some(cw) = self.database.changes().await.map(|mut ch| {
            let self2 = self.self_ref.clone();
            self.runtime.spawn(async move {
                loop {
                    if let Ok(change) = ch.recv().await {
                        if let Some(self2) = self2.upgrade() {
                            self2.reaper.add(
                                self2.runtime.spawn(self2.clone().handle_change_notification(change)),
                                Instant::now().checked_add(REQUEST_TIMEOUT).unwrap(),
                            );
                        } else {
                            break;
                        }
                    }
                }
            })
        }) {
            self.daemons.lock().unwrap().push(cw);
        }

        // Create background task to expire multicast subscriptions and recent authorizations.
        let self2 = self.self_ref.clone();
        self.daemons.lock().unwrap().push(self.runtime.spawn(async move {
            let sleep_duration = Duration::from_millis((protocol::VL2_DEFAULT_MULTICAST_LIKE_EXPIRE / 2).min(5000) as u64);
            loop {
                tokio::time::sleep(sleep_duration).await;

                if let Some(self2) = self2.upgrade() {
                    let time_ticks = ms_monotonic();
                    self2.multicast_authority.clean(time_ticks);
                    self2.recently_authorized.write().unwrap().retain(|_, by_network| {
                        by_network.retain(|_, timeout| *timeout > time_ticks);
                        !by_network.is_empty()
                    });
                } else {
                    break;
                }
            }
        }));
    }

    /// Compose and send network configuration packet.
    fn send_network_config(
        &self,
        peer: &Peer,
        config: &NetworkConfig,
        in_re_message_id: Option<u64>, // None for unsolicited push
    ) {
        if let Some(host_system) = self.service.read().unwrap().upgrade() {
            peer.send(
                host_system.as_ref(),
                host_system.node(),
                None,
                ms_monotonic(),
                |packet| -> Result<(), OutOfBoundsError> {
                    let payload_start = packet.len();

                    if let Some(in_re_message_id) = in_re_message_id {
                        let ok_header = packet.append_struct_get_mut::<protocol::OkHeader>()?;
                        ok_header.verb = protocol::message_type::VL1_OK;
                        ok_header.in_re_verb = protocol::message_type::VL2_NETWORK_CONFIG_REQUEST;
                        ok_header.in_re_message_id = in_re_message_id.to_be_bytes();
                    } else {
                        packet.append_u8(protocol::message_type::VL2_NETWORK_CONFIG)?;
                    }

                    if peer.is_v2() {
                        todo!()
                    } else {
                        let config_data = if let Some(config_dict) = config.v1_proto_to_dictionary(&self.local_identity) {
                            config_dict.to_bytes()
                        } else {
                            eprintln!("WARNING: unexpected error serializing network config into V1 format dictionary");
                            return Err(OutOfBoundsError); // abort
                        };
                        if config_data.len() > (u16::MAX as usize) {
                            eprintln!("WARNING: network config is larger than 65536 bytes!");
                            return Err(OutOfBoundsError); // abort
                        }

                        packet.append_u64(config.network_id.into())?;
                        packet.append_u16(config_data.len() as u16)?;
                        packet.append_bytes(config_data.as_slice())?;

                        // NOTE: V1 supports a bunch of other things like chunking but it was never truly used and is optional.
                        // Omit it here as it adds overhead and requires an extra signature we don't need since the other side
                        // knows this packet is coming directly from the controller. This stuff was originally designed to support
                        // a scatter-gather method of config distribution that was never implemented. V2 will just KISS and do
                        // controller clustering instead if we need scalability or more fault tolerance.
                    }

                    let new_payload_len = protocol::compress(&mut packet.as_bytes_mut()[payload_start..]);
                    packet.set_size(payload_start + new_payload_len);

                    Ok(())
                },
            );
        }
    }

    /// Send one or more revocation object(s) to a peer.
    fn send_revocations(&self, peer: &Peer, revocations: Vec<Revocation>) {
        if let Some(host_system) = self.service.read().unwrap().upgrade() {}
    }

    /// Called when the DB informs us of a change.
    async fn handle_change_notification(self: Arc<Self>, change: Change) {
        match change {
            Change::MemberAuthorized(_, _) => {}
            Change::MemberDeauthorized(network_id, node_id) => {
                if let Ok(Some(member)) = self.database.get_member(network_id, node_id).await {
                    if !member.authorized() {
                        // TODO
                    }
                }
            }
        }
    }

    /// Attempt to create a network configuration and return the result.
    ///
    /// This is the central function of the controller that looks up members, checks their
    /// permissions, and generates a network config and other credentials (or not).
    ///
    /// This may also return revocations. If it does these should be sent along with or right after
    /// the network config. This is for V1 nodes only, since V2 has another mechanism.
    ///
    /// An error is only returned if a database or other unusual error occurs. Otherwise a rejection
    /// reason is returned with None or an acceptance reason with a network configuration is returned.
    async fn get_network_config(
        self: &Arc<Self>,
        source_identity: &Identity,
        network_id: NetworkId,
        now: i64,
    ) -> Result<(AuthorizationResult, Option<NetworkConfig>, Option<Vec<vl2::v1::Revocation>>), Box<dyn Error + Send + Sync>> {
        let network = self.database.get_network(network_id).await?;
        if network.is_none() {
            return Ok((AuthorizationResult::Rejected, None, None));
        }
        let network = network.unwrap();

        let mut member = self.database.get_member(network_id, source_identity.address).await?;
        let mut member_changed = false;

        // If we have a member object and a pinned identity, check to make sure it matches. Also accept
        // upgraded identities to replace old versions if they are properly formed and inherit.
        if let Some(member) = member.as_mut() {
            if let Some(pinned_identity) = member.identity.as_ref() {
                if !pinned_identity.eq(&source_identity) {
                    return Ok((AuthorizationResult::RejectedIdentityMismatch, None, None));
                } else if source_identity.is_upgraded_from(pinned_identity) {
                    let _ = member.identity.replace(source_identity.clone_without_secret());
                    member_changed = true;
                }
            }
        }

        // This is the final verdict after everything has been checked.
        let mut authorization_result = AuthorizationResult::Rejected;

        // This is the main "authorized" flag on the member record. If it is true then
        // the member is allowed, but with the caveat that SSO must be checked if it's
        // enabled on the network. If this is false then the member is rejected unless
        // authorized by a token or unless it's a public network.
        let mut member_authorized = member.as_ref().map_or(false, |m| m.authorized());

        // If the member isn't authorized, check to see if it should be auto-authorized.
        if !member_authorized {
            if member.is_none() {
                if network.learn_members.unwrap_or(true) {
                    let _ = member.insert(Member::new_with_identity(source_identity.clone(), network_id));
                    member_changed = true;
                } else {
                    return Ok((AuthorizationResult::Rejected, None, None));
                }
            }

            if network.private {
                // TODO: check token authorization
            } else {
                authorization_result = AuthorizationResult::ApprovedOnPublicNetwork;
                member.as_mut().unwrap().last_authorized_time = Some(now);
                member_authorized = true;
                member_changed = true;
            }
        }

        let mut member = member.unwrap();

        // If the member is authorized set the final verdict to reflect this unless SSO (third party auth)
        // is enabled on the network and disagrees. Skip if the verdict is already one of the approved
        // values, which would indicate auth-authorization above.
        if member_authorized {
            if !authorization_result.approved() {
                // TODO: check SSO if enabled on network!
                authorization_result = AuthorizationResult::Approved;
            }
        } else {
            // This should not be able to be in approved state if member_authorized is still false.
            assert!(!authorization_result.approved());
        }

        let mut network_config = None;
        let mut revocations = None;
        if authorization_result.approved() {
            // We should not be able to make it here if this is still false.
            assert!(member_authorized);

            // Figure out TTL for credentials (time window in V1).
            let credential_ttl = network.credential_ttl.unwrap_or(CREDENTIAL_WINDOW_SIZE_DEFAULT);

            // Check and if necessary auto-assign static IPs for this member.
            member_changed |= network.check_zt_ip_assignments(self.database.as_ref(), &mut member).await;

            let mut nc = NetworkConfig::new(network_id, source_identity.address);

            nc.name = network.name.clone();
            nc.private = network.private;
            nc.timestamp = now;
            nc.credential_ttl = credential_ttl;
            nc.revision = Some(now as u64);
            nc.mtu = network.mtu.unwrap_or(ZEROTIER_VIRTUAL_NETWORK_DEFAULT_MTU as u16);
            nc.multicast_limit = network.multicast_limit.unwrap_or(DEFAULT_MULTICAST_LIMIT as u32);
            nc.multicast_like_expire = Some(protocol::VL2_DEFAULT_MULTICAST_LIKE_EXPIRE as u32);
            nc.routes = network.ip_routes;
            nc.static_ips = member.ip_assignments.clone();
            nc.rules = network.rules;
            nc.dns = network.dns;

            if network.min_supported_version.unwrap_or(0) < (protocol::PROTOCOL_VERSION_V2 as u32) {
                if let Some(com) =
                    vl2::v1::CertificateOfMembership::new(&self.local_identity, network_id, &source_identity, now, credential_ttl)
                {
                    let mut v1cred = V1Credentials {
                        certificate_of_membership: com,
                        certificates_of_ownership: Vec::new(),
                        tags: HashMap::new(),
                    };

                    if !nc.static_ips.is_empty() {
                        let mut coo = vl2::v1::CertificateOfOwnership::new(network_id, now, source_identity.address);
                        for ip in nc.static_ips.iter() {
                            coo.add_ip(ip);
                        }
                        if !coo.sign(&self.local_identity, &source_identity) {
                            return Ok((AuthorizationResult::RejectedDueToError, None, None));
                        }
                        v1cred.certificates_of_ownership.push(coo);
                    }

                    for (id, value) in member.tags.iter() {
                        let tag = vl2::v1::Tag::new(*id, *value, &self.local_identity, network_id, &source_identity, now);
                        if tag.is_none() {
                            return Ok((AuthorizationResult::RejectedDueToError, None, None));
                        }
                        let _ = v1cred.tags.insert(*id, tag.unwrap());
                    }

                    nc.v1_credentials = Some(v1cred);

                    // Staple a bunch of revocations for anyone deauthed that still might be in the window.
                    if let Ok(deauthed_members_still_in_window) = self
                        .database
                        .list_members_deauthorized_after(network.id, now - credential_ttl)
                        .await
                    {
                        if !deauthed_members_still_in_window.is_empty() {
                            let mut revs = Vec::with_capacity(deauthed_members_still_in_window.len());
                            for dm in deauthed_members_still_in_window.iter() {
                                if let Some(rev) = Revocation::new(
                                    network_id,
                                    now,
                                    *dm,
                                    source_identity.address,
                                    &self.local_identity,
                                    vl2::v1::CredentialType::CertificateOfMembership,
                                    false,
                                ) {
                                    revs.push(rev);
                                }
                            }
                            revocations = Some(revs);
                        }
                    }
                } else {
                    return Ok((AuthorizationResult::RejectedDueToError, None, None));
                }
            } else {
                // TODO: create V2 type credential for V2-only networks
                // TODO: populate node info for V2 networks
            }

            let _ = self
                .recently_authorized
                .write()
                .unwrap()
                .entry(source_identity.fingerprint)
                .or_default()
                .insert(network_id, ms_monotonic() + nc.credential_ttl);

            network_config = Some(nc);
        }

        if member_changed {
            self.database.save_member(member).await?;
        }

        Ok((authorization_result, network_config, revocations))
    }
}

impl InnerProtocol for Controller {
    fn handle_packet<HostSystemImpl: HostSystem + ?Sized>(
        &self,
        host_system: &HostSystemImpl,
        _: &Node,
        source: &Arc<Peer>,
        source_path: &Arc<Path>,
        source_hops: u8,
        message_id: u64,
        verb: u8,
        payload: &PacketBuffer,
        mut cursor: usize,
    ) -> PacketHandlerResult {
        match verb {
            protocol::message_type::VL2_NETWORK_CONFIG_REQUEST => {
                if !host_system.should_respond_to(&source.identity) {
                    return PacketHandlerResult::Ok; // handled and ignored
                }

                let network_id = payload.read_u64(&mut cursor);
                if network_id.is_err() {
                    return PacketHandlerResult::Error;
                }
                let network_id = NetworkId::from_u64(network_id.unwrap());
                if network_id.is_none() {
                    return PacketHandlerResult::Error;
                }
                let network_id = network_id.unwrap();

                debug_event!(
                    host_system,
                    "[vl2] NETWORK_CONFIG_REQUEST from {}({}) for {:0>16x}",
                    source.identity.address.to_string(),
                    source_path.endpoint.to_string(),
                    u64::from(network_id)
                );

                let meta_data = if (cursor + 2) < payload.len() {
                    let meta_data_len = payload.read_u16(&mut cursor);
                    if meta_data_len.is_err() {
                        return PacketHandlerResult::Error;
                    }
                    if let Ok(d) = payload.read_bytes(meta_data_len.unwrap() as usize, &mut cursor) {
                        let d = Dictionary::from_bytes(d);
                        if d.is_none() {
                            return PacketHandlerResult::Error;
                        }
                        d.unwrap()
                    } else {
                        return PacketHandlerResult::Error;
                    }
                } else {
                    Dictionary::new()
                };

                // Launch handler as an async background task.
                let (self2, source, source_remote_endpoint) =
                    (self.self_ref.upgrade().unwrap(), source.clone(), source_path.endpoint.clone());
                self.reaper.add(
                    self.runtime.spawn(async move {
                        let node_id = source.identity.address;
                        let node_fingerprint = Blob::from(source.identity.fingerprint);
                        let now = ms_since_epoch();

                        let (result, config) = match self2.get_network_config(&source.identity, network_id, now).await {
                            Result::Ok((result, Some(config), revocations)) => {
                                //println!("{}", serde_yaml::to_string(&config).unwrap());
                                self2.send_network_config(source.as_ref(), &config, Some(message_id));
                                if let Some(revocations) = revocations {
                                    self2.send_revocations(source.as_ref(), revocations);
                                }
                                (result, Some(config))
                            }
                            Result::Ok((result, None, _)) => (result, None),
                            Result::Err(e) => {
                                #[cfg(debug_assertions)]
                                let host = self2.service.read().unwrap().clone().upgrade().unwrap();
                                debug_event!(host, "[vl2] ERROR getting network config: {}", e.to_string());
                                return;
                            }
                        };

                        let _ = self2
                            .database
                            .log_request(RequestLogItem {
                                network_id,
                                node_id,
                                node_fingerprint,
                                controller_node_id: self2.local_identity.address,
                                metadata: if meta_data.is_empty() {
                                    Vec::new()
                                } else {
                                    meta_data.to_bytes()
                                },
                                peer_version: source.version(),
                                peer_protocol_version: source.protocol_version(),
                                timestamp: now,
                                source_remote_endpoint,
                                source_hops,
                                result,
                                config,
                            })
                            .await;
                    }),
                    Instant::now().checked_add(REQUEST_TIMEOUT).unwrap(),
                );

                PacketHandlerResult::Ok
            }

            protocol::message_type::VL2_MULTICAST_LIKE => {
                let auth = self.recently_authorized.read().unwrap();
                let time_ticks = ms_monotonic();
                self.multicast_authority.handle_vl2_multicast_like(
                    |network_id, identity| {
                        auth.get(&identity.fingerprint)
                            .map_or(false, |t| t.get(&network_id).map_or(false, |t| *t > time_ticks))
                    },
                    time_ticks,
                    source,
                    payload,
                    cursor,
                );
                PacketHandlerResult::Ok
            }

            protocol::message_type::VL2_MULTICAST_GATHER => {
                if let Some(service) = self.service.read().unwrap().upgrade() {
                    let auth = self.recently_authorized.read().unwrap();
                    let time_ticks = ms_monotonic();
                    self.multicast_authority.handle_vl2_multicast_gather(
                        |network_id, identity| {
                            auth.get(&identity.fingerprint)
                                .map_or(false, |t| t.get(&network_id).map_or(false, |t| *t > time_ticks))
                        },
                        time_ticks,
                        service.as_ref(),
                        service.node(),
                        source,
                        message_id,
                        payload,
                        cursor,
                    );
                }
                PacketHandlerResult::Ok
            }

            _ => PacketHandlerResult::NotHandled,
        }
    }
}

impl VL1AuthProvider for Controller {
    #[inline(always)]
    fn should_respond_to(&self, _: &Identity) -> bool {
        true
    }

    fn has_trust_relationship(&self, id: &Identity) -> bool {
        let time_ticks = ms_monotonic();
        self.recently_authorized
            .read()
            .unwrap()
            .get(&id.fingerprint)
            .map_or(false, |by_network| by_network.values().any(|t| *t > time_ticks))
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        for h in self.daemons.lock().unwrap().drain(..) {
            h.abort();
        }
    }
}
