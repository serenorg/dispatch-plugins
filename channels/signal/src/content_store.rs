//! Limit presage content persistence to authorized conversations.

use anyhow::{Result, anyhow};
use presage::AvatarBytes;
use presage::Manager;
use presage::libsignal_service::{
    Profile,
    content::{Content, ContentBody, SyncMessage, sync_message},
    libsignal_account_keys::AccountEntropyPool,
    prelude::{MasterKey, ProfileKey, Uuid},
    protocol::{IdentityKeyPair, SenderCertificate, ServiceId},
    zkgroup::GroupMasterKeyBytes,
};
use presage::manager::Registered;
use presage::manager::RegistrationData;
use presage::model::{contacts::Contact, groups::Group};
use presage::store::{ContentsStore, StateStore, StickerPack, Store, Thread};
use presage_store_sqlite::{SqliteStore, SqliteStoreError};
use std::ops::RangeBounds;

use crate::protocol::SignalChannelPolicy;

#[derive(Debug, Clone, Copy)]
enum ContentScope {
    All,
    SelfChat { account: ServiceId, device_id: u32 },
}

#[derive(Debug, Clone)]
pub struct SignalStore {
    inner: SqliteStore,
    scope: ContentScope,
}

impl SignalStore {
    pub async fn load_registered(
        inner: SqliteStore,
        policy: &SignalChannelPolicy,
    ) -> Result<Manager<Self, Registered>> {
        let manager =
            Manager::<Self, Registered>::load_registered(Self::for_policy(inner, policy)?)
                .await
                .map_err(|error| anyhow!("failed to load Signal session: {error}"))?;
        if let Some((account, device)) = policy.self_chat_account() {
            let data = manager.registration_data();
            if data.service_ids.aci.to_string() != account || data.device_id.unwrap_or(1) != device
            {
                return Err(anyhow!(
                    "Signal linked account changed; reconfigure the binding"
                ));
            }
        }
        Ok(manager)
    }

    pub fn for_policy(inner: SqliteStore, policy: &SignalChannelPolicy) -> Result<Self> {
        let scope = match policy.self_chat_account() {
            Some((account, device_id)) => ContentScope::SelfChat {
                account: ServiceId::parse_from_service_id_string(account)
                    .ok_or_else(|| anyhow!("Signal self-chat account is invalid"))?,
                device_id,
            },
            None => ContentScope::All,
        };
        Ok(Self { inner, scope })
    }

    fn is_unrestricted(&self) -> bool {
        matches!(self.scope, ContentScope::All)
    }

    fn allows_thread(&self, thread: &Thread) -> bool {
        match self.scope {
            ContentScope::All => true,
            ContentScope::SelfChat { account, .. } => *thread == Thread::Contact(account),
        }
    }

    fn scoped_message(&self, thread: &Thread, mut content: Content) -> Option<Content> {
        let ContentScope::SelfChat {
            account: own,
            device_id,
        } = self.scope
        else {
            return Some(content);
        };
        if !self.allows_thread(thread)
            || content.metadata.sender != own
            || content.metadata.destination != own
            || content.metadata.was_plaintext
        {
            return None;
        }
        match content.body {
            ContentBody::DataMessage(ref message)
                if message.group_v2.is_none()
                    && u32::from(content.metadata.sender_device) == device_id => {}
            ContentBody::SynchronizeMessage(sync) => {
                let sent = sync.sent?;
                if sent.parse_destination_service_id()? != own
                    || u32::from(content.metadata.sender_device) == device_id
                    || i64::try_from(sent.timestamp?).is_err()
                {
                    return None;
                }
                let message = sent.message?;
                if message.group_v2.is_some() {
                    return None;
                }
                // A sent transcript can share its envelope with unrelated sync data.
                content.body = ContentBody::SynchronizeMessage(SyncMessage {
                    sent: Some(sync_message::Sent {
                        destination_service_id: Some(own.service_id_string()),
                        timestamp: sent.timestamp,
                        message: Some(message),
                        ..Default::default()
                    }),
                    ..Default::default()
                });
            }
            _ => return None,
        }
        Some(content)
    }

    fn timer_snapshot(&self, thread: &Thread, content: Content) -> Option<Content> {
        let mut content = self.scoped_message(thread, content)?;
        let timer_fields = |message: presage::libsignal_service::content::DataMessage| {
            presage::libsignal_service::content::DataMessage {
                timestamp: message.timestamp,
                expire_timer: message.expire_timer,
                expire_timer_version: message.expire_timer_version,
                ..Default::default()
            }
        };
        match &mut content.body {
            ContentBody::DataMessage(message) => *message = timer_fields(std::mem::take(message)),
            ContentBody::SynchronizeMessage(sync) => {
                let sent = sync.sent.as_mut()?;
                sent.message = Some(timer_fields(sent.message.take()?));
            }
            _ => return None,
        }
        Some(content)
    }

    fn snapshot_timer(&self, thread: &Thread, content: Content) -> Option<(u32, u32)> {
        let snapshot = self.timer_snapshot(thread, content.clone())?;
        if content.body.into_proto() != snapshot.body.clone().into_proto() {
            return None;
        }
        let message = match snapshot.body {
            ContentBody::DataMessage(message) => message,
            ContentBody::SynchronizeMessage(sync) => sync.sent?.message?,
            _ => return None,
        };
        Some((
            message.expire_timer.unwrap_or_default(),
            message.expire_timer_version.unwrap_or(1),
        ))
    }
}

// Session and protocol state remain intact; content policy does not grant access to other chats.
impl Store for SignalStore {
    type Error = SqliteStoreError;
    type AciStore = <SqliteStore as Store>::AciStore;
    type PniStore = <SqliteStore as Store>::PniStore;

    async fn clear(&mut self) -> Result<(), Self::Error> {
        self.inner.clear().await
    }

    fn aci_protocol_store(&self) -> Self::AciStore {
        self.inner.aci_protocol_store()
    }

    fn pni_protocol_store(&self) -> Self::PniStore {
        self.inner.pni_protocol_store()
    }
}

impl StateStore for SignalStore {
    type StateStoreError = SqliteStoreError;

    async fn load_registration_data(
        &self,
    ) -> Result<Option<RegistrationData>, Self::StateStoreError> {
        self.inner.load_registration_data().await
    }

    async fn set_aci_identity_key_pair(
        &self,
        key: IdentityKeyPair,
    ) -> Result<(), Self::StateStoreError> {
        self.inner.set_aci_identity_key_pair(key).await
    }

    async fn set_pni_identity_key_pair(
        &self,
        key: IdentityKeyPair,
    ) -> Result<(), Self::StateStoreError> {
        self.inner.set_pni_identity_key_pair(key).await
    }

    async fn save_registration_data(
        &mut self,
        data: &RegistrationData,
    ) -> Result<(), Self::StateStoreError> {
        self.inner.save_registration_data(data).await
    }

    async fn sender_certificate(&self) -> Result<Option<SenderCertificate>, Self::StateStoreError> {
        self.inner.sender_certificate().await
    }

    async fn save_sender_certificate(
        &self,
        certificate: &SenderCertificate,
    ) -> Result<(), Self::StateStoreError> {
        self.inner.save_sender_certificate(certificate).await
    }

    async fn is_registered(&self) -> bool {
        self.inner.is_registered().await
    }

    async fn clear_registration(&mut self) -> Result<(), Self::StateStoreError> {
        self.inner.clear_registration().await
    }

    async fn fetch_master_key(&self) -> Result<Option<MasterKey>, Self::StateStoreError> {
        self.inner.fetch_master_key().await
    }

    async fn store_master_key(&self, key: Option<&MasterKey>) -> Result<(), Self::StateStoreError> {
        self.inner.store_master_key(key).await
    }

    async fn fetch_account_entropy_pool(
        &self,
    ) -> Result<Option<AccountEntropyPool>, Self::StateStoreError> {
        self.inner.fetch_account_entropy_pool().await
    }

    async fn store_account_entropy_pool(
        &self,
        pool: Option<&AccountEntropyPool>,
    ) -> Result<(), Self::StateStoreError> {
        self.inner.store_account_entropy_pool(pool).await
    }
}

impl ContentsStore for SignalStore {
    type ContentsStoreError = SqliteStoreError;
    type ContactsIter = <SqliteStore as ContentsStore>::ContactsIter;
    type GroupsIter = <SqliteStore as ContentsStore>::GroupsIter;
    type MessagesIter = <SqliteStore as ContentsStore>::MessagesIter;
    type StickerPacksIter = <SqliteStore as ContentsStore>::StickerPacksIter;

    async fn clear_profiles(&mut self) -> Result<(), Self::ContentsStoreError> {
        if self.is_unrestricted() {
            self.inner.clear_profiles().await
        } else {
            Ok(())
        }
    }

    async fn clear_contents(&mut self) -> Result<(), Self::ContentsStoreError> {
        match self.scope {
            ContentScope::All => self.inner.clear_contents().await,
            ContentScope::SelfChat { .. } => Ok(()),
        }
    }

    async fn clear_messages(&mut self) -> Result<(), Self::ContentsStoreError> {
        match self.scope {
            ContentScope::All => self.inner.clear_messages().await,
            ContentScope::SelfChat { .. } => Ok(()),
        }
    }

    async fn clear_thread(&mut self, thread: &Thread) -> Result<(), Self::ContentsStoreError> {
        if self.is_unrestricted() {
            self.inner.clear_thread(thread).await
        } else {
            Ok(())
        }
    }

    async fn save_message(
        &self,
        thread: &Thread,
        message: Content,
    ) -> Result<(), Self::ContentsStoreError> {
        if self.is_unrestricted() {
            return self.inner.save_message(thread, message).await;
        }
        match self.timer_snapshot(thread, message) {
            Some(message) => {
                let timestamp = u64::try_from(message.metadata.timestamp.timestamp_millis())
                    .map_err(|_| SqliteStoreError::InvalidFormat)?;
                if let Some(previous) = self.inner.message(thread, timestamp).await?
                    && self.snapshot_timer(thread, previous).is_none()
                {
                    return Err(SqliteStoreError::InvalidFormat);
                }
                self.inner.save_message(thread, message).await
            }
            None => Ok(()),
        }
    }

    async fn delete_message(
        &mut self,
        thread: &Thread,
        timestamp: u64,
    ) -> Result<bool, Self::ContentsStoreError> {
        if !self.is_unrestricted() {
            return Ok(false);
        }
        self.inner.delete_message(thread, timestamp).await
    }

    async fn message(
        &self,
        thread: &Thread,
        timestamp: u64,
    ) -> Result<Option<Content>, Self::ContentsStoreError> {
        if !self.is_unrestricted() {
            return Ok(None);
        }
        self.inner.message(thread, timestamp).await
    }

    async fn thread_for_sender_and_timestamp(
        &self,
        sender: &ServiceId,
        timestamp: u64,
    ) -> Result<Option<Thread>, Self::ContentsStoreError> {
        if !self.is_unrestricted() {
            return Ok(None);
        }
        self.inner
            .thread_for_sender_and_timestamp(sender, timestamp)
            .await
    }

    async fn messages(
        &self,
        thread: &Thread,
        range: impl RangeBounds<u64>,
    ) -> Result<Self::MessagesIter, Self::ContentsStoreError> {
        if !self.is_unrestricted() {
            return Ok(Box::new(std::iter::empty()));
        }
        self.inner.messages(thread, range).await
    }

    async fn clear_contacts(&mut self) -> Result<(), Self::ContentsStoreError> {
        if self.is_unrestricted() {
            self.inner.clear_contacts().await
        } else {
            Ok(())
        }
    }

    async fn expire_timer(
        &self,
        thread: &Thread,
    ) -> Result<Option<(u32, u32)>, Self::ContentsStoreError> {
        if self.is_unrestricted() {
            return self.inner.expire_timer(thread).await;
        }
        if !self.allows_thread(thread) {
            return Ok(None);
        }
        let messages = self.inner.messages(thread, ..).await?;
        let mut timer = None;
        for content in messages {
            let Some((duration, version)) = self.snapshot_timer(thread, content?) else {
                continue;
            };
            if timer.is_none_or(|(_, current)| version > current) {
                timer = Some((duration, version));
            }
        }
        Ok(timer)
    }

    async fn save_contact(&mut self, contact: &Contact) -> Result<(), Self::ContentsStoreError> {
        if self.is_unrestricted() {
            self.inner.save_contact(contact).await
        } else {
            Ok(())
        }
    }

    async fn contacts(&self) -> Result<Self::ContactsIter, Self::ContentsStoreError> {
        if self.is_unrestricted() {
            self.inner.contacts().await
        } else {
            Ok(Box::new(std::iter::empty()))
        }
    }

    async fn contact_by_id(
        &self,
        id: &ServiceId,
    ) -> Result<Option<Contact>, Self::ContentsStoreError> {
        if self.is_unrestricted() {
            self.inner.contact_by_id(id).await
        } else {
            Ok(None)
        }
    }

    async fn clear_groups(&mut self) -> Result<(), Self::ContentsStoreError> {
        if self.is_unrestricted() {
            self.inner.clear_groups().await
        } else {
            Ok(())
        }
    }

    async fn save_group(
        &self,
        key: GroupMasterKeyBytes,
        group: impl Into<Group>,
    ) -> Result<(), Self::ContentsStoreError> {
        if self.is_unrestricted() {
            self.inner.save_group(key, group).await
        } else {
            Ok(())
        }
    }

    async fn groups(&self) -> Result<Self::GroupsIter, Self::ContentsStoreError> {
        if self.is_unrestricted() {
            self.inner.groups().await
        } else {
            Ok(Box::new(std::iter::empty()))
        }
    }

    async fn group(
        &self,
        key: GroupMasterKeyBytes,
    ) -> Result<Option<Group>, Self::ContentsStoreError> {
        if self.is_unrestricted() {
            self.inner.group(key).await
        } else {
            Ok(None)
        }
    }

    async fn save_group_avatar(
        &self,
        key: GroupMasterKeyBytes,
        avatar: &AvatarBytes,
    ) -> Result<(), Self::ContentsStoreError> {
        if self.is_unrestricted() {
            self.inner.save_group_avatar(key, avatar).await
        } else {
            Ok(())
        }
    }

    async fn group_avatar(
        &self,
        key: GroupMasterKeyBytes,
    ) -> Result<Option<AvatarBytes>, Self::ContentsStoreError> {
        if self.is_unrestricted() {
            self.inner.group_avatar(key).await
        } else {
            Ok(None)
        }
    }

    async fn upsert_profile_key(
        &mut self,
        uuid: &Uuid,
        key: ProfileKey,
    ) -> Result<bool, Self::ContentsStoreError> {
        if self.is_unrestricted() {
            self.inner.upsert_profile_key(uuid, key).await
        } else {
            Ok(false)
        }
    }

    async fn profile_key(
        &self,
        id: &ServiceId,
    ) -> Result<Option<ProfileKey>, Self::ContentsStoreError> {
        if self.is_unrestricted() {
            self.inner.profile_key(id).await
        } else {
            Ok(None)
        }
    }

    async fn save_profile(
        &mut self,
        uuid: Uuid,
        key: ProfileKey,
        profile: Profile,
    ) -> Result<(), Self::ContentsStoreError> {
        if self.is_unrestricted() {
            self.inner.save_profile(uuid, key, profile).await
        } else {
            Ok(())
        }
    }

    async fn profile(
        &self,
        uuid: Uuid,
        key: ProfileKey,
    ) -> Result<Option<Profile>, Self::ContentsStoreError> {
        if self.is_unrestricted() {
            self.inner.profile(uuid, key).await
        } else {
            Ok(None)
        }
    }

    async fn save_profile_avatar(
        &mut self,
        uuid: Uuid,
        key: ProfileKey,
        avatar: &AvatarBytes,
    ) -> Result<(), Self::ContentsStoreError> {
        if self.is_unrestricted() {
            self.inner.save_profile_avatar(uuid, key, avatar).await
        } else {
            Ok(())
        }
    }

    async fn profile_avatar(
        &self,
        uuid: Uuid,
        key: ProfileKey,
    ) -> Result<Option<AvatarBytes>, Self::ContentsStoreError> {
        if self.is_unrestricted() {
            self.inner.profile_avatar(uuid, key).await
        } else {
            Ok(None)
        }
    }

    async fn add_sticker_pack(
        &mut self,
        pack: &StickerPack,
    ) -> Result<(), Self::ContentsStoreError> {
        if self.is_unrestricted() {
            self.inner.add_sticker_pack(pack).await
        } else {
            Ok(())
        }
    }

    async fn sticker_pack(
        &self,
        id: &[u8],
    ) -> Result<Option<StickerPack>, Self::ContentsStoreError> {
        if self.is_unrestricted() {
            self.inner.sticker_pack(id).await
        } else {
            Ok(None)
        }
    }

    async fn remove_sticker_pack(&mut self, id: &[u8]) -> Result<bool, Self::ContentsStoreError> {
        if self.is_unrestricted() {
            self.inner.remove_sticker_pack(id).await
        } else {
            Ok(false)
        }
    }

    async fn sticker_packs(&self) -> Result<Self::StickerPacksIter, Self::ContentsStoreError> {
        if self.is_unrestricted() {
            self.inner.sticker_packs().await
        } else {
            Ok(Box::new(std::iter::empty()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ChannelConfig, DmPolicy};
    use presage::libsignal_service::content::{DataMessage, Metadata};
    use presage::libsignal_service::proto::{GroupContextV2, sync_message::Request};
    use presage::model::identity::OnNewIdentity;

    const OWN: &str = "00000000-0000-0000-0000-000000000001";
    const OTHER: &str = "00000000-0000-0000-0000-000000000002";

    fn id(raw: &str) -> ServiceId {
        ServiceId::parse_from_service_id_string(raw).unwrap()
    }

    async fn self_chat_store() -> SignalStore {
        let inner = SqliteStore::open("sqlite::memory:", OnNewIdentity::Trust)
            .await
            .unwrap();
        let policy = SignalChannelPolicy::for_self_chat(
            &ChannelConfig {
                self_chat_only: true,
                dm_policy: DmPolicy::Open,
                ..Default::default()
            },
            OWN,
            7,
        )
        .unwrap();
        SignalStore::for_policy(inner, &policy).unwrap()
    }

    fn transcript(destination: ServiceId) -> Content {
        Content::from_body(
            ContentBody::SynchronizeMessage(SyncMessage {
                sent: Some(sync_message::Sent {
                    destination_service_id: Some(destination.service_id_string()),
                    timestamp: Some(42),
                    message: Some(DataMessage {
                        body: Some("hello".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            Metadata {
                sender: id(OWN),
                destination: id(OWN),
                sender_device: 1_u32.try_into().unwrap(),
                timestamp: Default::default(),
                server_timestamp: Default::default(),
                needs_receipt: false,
                unidentified_sender: false,
                was_plaintext: false,
                server_guid: None,
            },
        )
    }

    #[tokio::test]
    async fn unrelated_messages_never_reach_the_underlying_store() {
        let store = self_chat_store().await;
        let own = Thread::Contact(id(OWN));
        let other = Thread::Contact(id(OTHER));
        let mut unrelated = transcript(id(OTHER));
        store.save_message(&own, unrelated.clone()).await.unwrap();
        store.save_message(&other, unrelated.clone()).await.unwrap();
        unrelated.metadata.sender = id(OTHER);
        unrelated.metadata.destination = id(OTHER);
        unrelated.body = ContentBody::DataMessage(DataMessage {
            body: Some("private".to_string()),
            ..Default::default()
        });
        store.save_message(&other, unrelated).await.unwrap();
        assert_eq!(store.inner.messages(&other, ..).await.unwrap().count(), 0);
        assert_eq!(store.inner.messages(&own, ..).await.unwrap().count(), 0);
        store.save_message(&own, transcript(id(OWN))).await.unwrap();
        assert_eq!(store.inner.messages(&own, ..).await.unwrap().count(), 1);
        let saved = store
            .inner
            .messages(&own, ..)
            .await
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        let ContentBody::SynchronizeMessage(sync) = saved.body else {
            panic!("expected timer snapshot");
        };
        let data = sync.sent.unwrap().message.unwrap();
        assert!(data.body.is_none());
        assert!(data.attachments.is_empty());
        assert!(store.message(&own, 0).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn self_chat_filters_message_provenance_and_extra_sync_data() {
        let store = self_chat_store().await;
        let own = Thread::Contact(id(OWN));
        for invalid in 0..5 {
            let mut message = transcript(id(OWN));
            let ContentBody::SynchronizeMessage(ref mut sync) = message.body else {
                unreachable!()
            };
            match invalid {
                0 => message.metadata.sender_device = 7_u32.try_into().unwrap(),
                1 => message.metadata.was_plaintext = true,
                2 => message.metadata.sender = id(OTHER),
                3 => sync.sent.as_mut().unwrap().timestamp = None,
                4 => {
                    sync.sent
                        .as_mut()
                        .unwrap()
                        .message
                        .as_mut()
                        .unwrap()
                        .group_v2 = Some(GroupContextV2::default())
                }
                _ => unreachable!(),
            }
            store.save_message(&own, message).await.unwrap();
        }
        assert_eq!(store.inner.messages(&own, ..).await.unwrap().count(), 0);
        let mut message = transcript(id(OWN));
        let ContentBody::SynchronizeMessage(ref mut sync) = message.body else {
            unreachable!()
        };
        sync.request = Some(Request::default());
        store.save_message(&own, message).await.unwrap();
        let saved = store
            .inner
            .messages(&own, ..)
            .await
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        let ContentBody::SynchronizeMessage(sync) = saved.body else {
            panic!("expected sent transcript")
        };
        assert!(sync.sent.is_some());
        assert!(sync.request.is_none());
    }

    #[tokio::test]
    async fn unrelated_metadata_is_not_persisted() {
        let mut store = self_chat_store().await;
        let contact = Contact {
            uuid: id(OTHER).raw_uuid(),
            phone_number: None,
            name: "private contact".to_string(),
            verified: Default::default(),
            profile_key: vec![0; 32],
            expire_timer: 30,
            expire_timer_version: 1,
            inbox_position: 0,
            avatar: None,
        };
        store.save_contact(&contact).await.unwrap();
        let key = ProfileKey::create([0; 32]);
        store.upsert_profile_key(&contact.uuid, key).await.unwrap();
        store
            .save_profile(
                contact.uuid,
                key,
                Profile {
                    name: None,
                    about: Some("private profile".to_string()),
                    about_emoji: None,
                    avatar: None,
                    unrestricted_unidentified_access: false,
                },
            )
            .await
            .unwrap();
        store
            .save_profile_avatar(contact.uuid, key, &vec![1, 2, 3])
            .await
            .unwrap();
        store
            .save_group_avatar([0; 32], &vec![1, 2, 3])
            .await
            .unwrap();
        let pack = StickerPack {
            id: vec![1],
            key: vec![2],
            manifest: presage::store::StickerPackManifest {
                title: "private pack".to_string(),
                author: "owner".to_string(),
                cover: None,
                stickers: vec![],
            },
        };
        store.add_sticker_pack(&pack).await.unwrap();
        assert_eq!(store.inner.contacts().await.unwrap().count(), 0);
        assert!(store.inner.profile_key(&id(OTHER)).await.unwrap().is_none());
        assert!(
            store
                .inner
                .profile(contact.uuid, key)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .inner
                .profile_avatar(contact.uuid, key)
                .await
                .unwrap()
                .is_none()
        );
        assert!(store.inner.group_avatar([0; 32]).await.unwrap().is_none());
        assert_eq!(store.inner.sticker_packs().await.unwrap().count(), 0);
    }

    #[tokio::test]
    async fn existing_unrelated_records_are_hidden_but_not_deleted() {
        let mut store = self_chat_store().await;
        let own = Thread::Contact(id(OWN));
        let mut unrelated = transcript(id(OTHER));
        unrelated.metadata.sender = id(OWN);
        store.inner.save_message(&own, unrelated).await.unwrap();
        assert!(store.message(&own, 0).await.unwrap().is_none());
        assert_eq!(store.messages(&own, ..).await.unwrap().count(), 0);
        assert!(
            store
                .thread_for_sender_and_timestamp(&id(OWN), 0)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            !store
                .delete_message(&Thread::Contact(id(OTHER)), 0)
                .await
                .unwrap()
        );
        assert!(!store.delete_message(&own, 0).await.unwrap());
        store.clear_thread(&own).await.unwrap();
        store.clear_messages().await.unwrap();
        store.clear_contents().await.unwrap();
        assert!(store.save_message(&own, transcript(id(OWN))).await.is_err());
        assert_eq!(store.inner.messages(&own, ..).await.unwrap().count(), 1);
    }

    #[tokio::test]
    async fn self_chat_timer_comes_only_from_accepted_content() {
        let store = self_chat_store().await;
        let own = Thread::Contact(id(OWN));
        let mut message = transcript(id(OWN));
        let ContentBody::SynchronizeMessage(ref mut sync) = message.body else {
            unreachable!()
        };
        let data = sync.sent.as_mut().unwrap().message.as_mut().unwrap();
        data.expire_timer = Some(60);
        data.expire_timer_version = Some(3);
        store.save_message(&own, message).await.unwrap();
        assert_eq!(store.expire_timer(&own).await.unwrap(), Some((60, 3)));
        let mut stale = transcript(id(OWN));
        stale.metadata.timestamp = "1970-01-01T00:00:00.001Z".parse().unwrap();
        let ContentBody::SynchronizeMessage(ref mut sync) = stale.body else {
            unreachable!();
        };
        let data = sync.sent.as_mut().unwrap().message.as_mut().unwrap();
        data.expire_timer = Some(5);
        data.expire_timer_version = Some(2);
        store.save_message(&own, stale).await.unwrap();
        assert_eq!(store.expire_timer(&own).await.unwrap(), Some((60, 3)));
        assert_eq!(
            store
                .expire_timer(&Thread::Contact(id(OTHER)))
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn outgoing_self_chat_text_and_attachments_are_not_saved() {
        let store = self_chat_store().await;
        let own = Thread::Contact(id(OWN));
        let mut message = transcript(id(OWN));
        message.metadata.sender_device = 7_u32.try_into().unwrap();
        message.body = ContentBody::DataMessage(DataMessage {
            body: Some("reply".to_string()),
            attachments: vec![presage::libsignal_service::content::AttachmentPointer::default()],
            profile_key: Some(vec![0; 32]),
            expire_timer: Some(60),
            expire_timer_version: Some(1),
            ..Default::default()
        });
        store.save_message(&own, message).await.unwrap();
        let saved = store
            .inner
            .messages(&own, ..)
            .await
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        let ContentBody::DataMessage(data) = saved.body else {
            panic!("expected timer snapshot");
        };
        assert_eq!(
            data,
            DataMessage {
                expire_timer: Some(60),
                expire_timer_version: Some(1),
                ..Default::default()
            }
        );
        assert_eq!(store.messages(&own, ..).await.unwrap().count(), 0);
    }

    #[tokio::test]
    async fn ordinary_mode_keeps_existing_storage_behavior() {
        let inner = SqliteStore::open("sqlite::memory:", OnNewIdentity::Trust)
            .await
            .unwrap();
        let policy = SignalChannelPolicy::from_config(&ChannelConfig {
            dm_policy: DmPolicy::Open,
            ..Default::default()
        })
        .unwrap();
        let store = SignalStore::for_policy(inner, &policy).unwrap();
        let other = Thread::Contact(id(OTHER));
        store
            .save_message(&other, transcript(id(OTHER)))
            .await
            .unwrap();
        assert_eq!(store.messages(&other, ..).await.unwrap().count(), 1);
    }
}
