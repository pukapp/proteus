// This Source Code Form is subject to the terms of
// the Mozilla Public License, v. 2.0. If a copy of
// the MPL was not distributed with this file, You
// can obtain one at http://mozilla.org/MPL/2.0/.

use hkdf::{Input, Info, Salt};
use internal::derived::{DerivedSecrets, CipherKey, MacKey};
use internal::keys;
use internal::keys::{IdentityKey, IdentityKeyPair, PreKeyBundle, PreKey, PreKeyId};
use internal::keys::{KeyPair, PublicKey};
use internal::message::{Counter, PreKeyMessage, Envelope, Message, CipherMessage};
use internal::util;
use std::cmp::{Ord, Ordering};
use std::collections::RingBuf;
use std::error::{Error, FromError};
use std::fmt;
use std::iter::count;
use std::vec::Vec;

pub mod binary;

// Root key /////////////////////////////////////////////////////////////////

#[derive(Clone)]
pub struct RootKey {
    key: CipherKey
}

impl RootKey {
    pub fn from_cipher_key(k: CipherKey) -> RootKey {
        RootKey { key: k }
    }

    pub fn dh_ratchet(&self, ours: &KeyPair, theirs: &PublicKey) -> (RootKey, ChainKey) {
        let secret = ours.secret_key.shared_secret(theirs);
        let dsecs  = DerivedSecrets::kdf(Input(secret.as_slice()),
                                         Salt(self.key.as_slice()),
                                         Info(b"dh_ratchet"));
        (RootKey::from_cipher_key(dsecs.cipher_key), ChainKey::from_mac_key(dsecs.mac_key, Counter::zero()))
    }
}

// Chain key /////////////////////////////////////////////////////////////////

#[derive(Clone)]
pub struct ChainKey {
    key: MacKey,
    idx: Counter
}

impl ChainKey {
    pub fn from_mac_key(k: MacKey, idx: Counter) -> ChainKey {
        ChainKey { key: k, idx: idx }
    }

    pub fn next(&self) -> ChainKey {
        ChainKey {
            key: MacKey::new(self.key.sign(b"1").to_bytes()),
            idx: self.idx.next()
        }
    }

    pub fn message_keys(&self) -> MessageKeys {
        let base  = self.key.sign(b"0");
        let dsecs = DerivedSecrets::kdf_without_salt(Input(base.as_slice()),
                                                     Info(b"hash_ratchet"));
        MessageKeys {
            cipher_key: dsecs.cipher_key,
            mac_key:    dsecs.mac_key,
            counter:    self.idx
        }
    }
}

// Send Chain ///////////////////////////////////////////////////////////////

#[derive(Clone)]
pub struct SendChain {
    chain_key:   ChainKey,
    ratchet_key: KeyPair
}

impl SendChain {
    pub fn new(ck: ChainKey, rk: KeyPair) -> SendChain {
        SendChain { chain_key: ck, ratchet_key: rk }
    }
}

// Receive Chain ////////////////////////////////////////////////////////////

#[derive(Clone)]
pub struct RecvChain {
    chain_key:   ChainKey,
    ratchet_key: PublicKey
}

impl RecvChain {
    pub fn new(ck: ChainKey, rk: PublicKey) -> RecvChain {
        RecvChain { chain_key: ck, ratchet_key: rk }
    }
}

// Message Keys /////////////////////////////////////////////////////////////

#[derive(Clone)]
pub struct MessageKeys {
    cipher_key: CipherKey,
    mac_key:    MacKey,
    counter:    Counter
}

impl MessageKeys {
    fn encrypt(&self, plain_text: &[u8]) -> Vec<u8> {
        self.cipher_key.encrypt(plain_text, &self.counter.as_nonce())
    }

    fn decrypt(&self, cipher_text: &[u8]) -> Vec<u8> {
        self.cipher_key.decrypt(cipher_text, &self.counter.as_nonce())
    }
}

// Store ////////////////////////////////////////////////////////////////////

pub trait PreKeyStore<E> {
    fn prekey(&self, id: PreKeyId) -> Result<Option<PreKey>, E>;
    fn remove(&mut self, id: PreKeyId) -> Result<(), E>;
}

// Session //////////////////////////////////////////////////////////////////

const MAX_RECV_CHAINS:    usize = 5;
const MAX_COUNTER_GAP:    usize = 1000;
const MAX_SESSION_STATES: usize = 100;

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum Version { V1 }

pub struct Session {
    version:         Version,
    local_identity:  IdentityKeyPair,
    remote_identity: IdentityKey,
    pending_prekey:  Option<(PreKeyId, PublicKey)>,
    session_states:  RingBuf<SessionState>
}

struct AliceParams<'r> {
    alice_ident:   &'r IdentityKeyPair,
    alice_base:    &'r KeyPair,
    bob:           &'r PreKeyBundle
}

struct BobParams<'r> {
    bob_ident:     &'r IdentityKeyPair,
    bob_prekey:    KeyPair,
    alice_ident:   &'r IdentityKey,
    alice_base:    &'r PublicKey
}

impl Session {
    pub fn init_from_prekey<'r>(alice: &'r IdentityKeyPair, pk: PreKeyBundle) -> Session {
        let alice_base = KeyPair::new();
        let mut states = RingBuf::new();
        states.push_front(SessionState::init_as_alice(AliceParams {
            alice_ident:   alice,
            alice_base:    &alice_base,
            bob:           &pk
        }));

        Session {
            version:         Version::V1,
            local_identity:  alice.clone(),
            remote_identity: pk.identity_key,
            pending_prekey:  Some((pk.prekey_id, alice_base.public_key)),
            session_states:  states,
        }
    }

    pub fn init_from_message<'r, E: Error>(ours: &'r IdentityKeyPair, store: &mut PreKeyStore<E>, env: &Envelope) -> Result<(Session, Vec<u8>), DecryptError<E>> {
        let msg = match *env.message() {
            Message::Plain(_)     => return Err(DecryptError::InvalidMessage),
            Message::Keyed(ref m) => m
        };

        let mut session = Session {
            version:         Version::V1,
            local_identity:  ours.clone(),
            remote_identity: msg.identity_key,
            pending_prekey:  None,
            session_states:  RingBuf::new()
        };

        let plain = try!(session.decrypt(store, env));
        assert!(!session.session_states.is_empty());

        Ok((session, plain))
    }

    pub fn encrypt(&mut self, plain: &[u8]) -> Envelope {
        assert!(!self.session_states.is_empty());

        let pending  = self.pending_prekey;
        let identity = self.local_identity.public_key;
        let state    = self.session_states.front_mut().unwrap();

        state.encrypt(identity, &pending, plain)
    }

    pub fn decrypt<E: Error>(&mut self, store: &mut PreKeyStore<E>, env: &Envelope) -> Result<Vec<u8>, DecryptError<E>> {
        let mesg = match *env.message() {
            Message::Plain(ref m) => m,
            Message::Keyed(ref m) => {
                if m.identity_key != self.remote_identity {
                    return Err(DecryptError::RemoteIdentityChanged)
                }
                try!(self.unpack(store, m))
            }
        };

        assert!(!self.session_states.is_empty());

        // try first session state
        let mut first_state = self.session_states[0].clone();
        let first_result    = first_state.decrypt(env, mesg);

        if first_result.is_ok() {
            self.session_states[0] = first_state;
            self.pending_prekey    = None;
            return first_result
        }

        // try remaining session states
        let result =
            self.session_states.iter().skip(1).zip(count(1, 1)).map(|(s0, i)| {
                let mut s1 = s0.clone();
                let result = s1.decrypt(env, mesg);
                if result.is_ok() {
                    Some((result, s1, i))
                } else {
                    None
                }
            }).find(|x| x.is_some()).and_then(|x| x);

        match result {
            Some((plain, new_state, ix)) => {
                self.session_states.remove(ix);
                self.session_states.push_front(new_state);
                self.pending_prekey = None;
                plain
            }
            None => first_result
        }
    }

    fn unpack<'s, E: Error>(&mut self, store: &mut PreKeyStore<E>, m: &'s PreKeyMessage) -> Result<&'s CipherMessage, DecryptError<E>> {
        try!(store.prekey(m.prekey_id)).map(|prekey| {
            let new_state = SessionState::init_as_bob(BobParams {
                bob_ident:   &self.local_identity,
                bob_prekey:  prekey.key_pair,
                alice_ident: &m.identity_key,
                alice_base:  &m.base_key
            });
            self.session_states.push_front(new_state);
            if self.session_states.len() > MAX_SESSION_STATES {
                self.session_states.pop_back();
            }
        });
        if m.prekey_id != keys::MAX_PREKEY_ID {
            try!(store.remove(m.prekey_id));
        }
        Ok(&m.message)
    }

    pub fn encode(&self) -> Vec<u8> {
        util::encode(self, binary::enc_session).unwrap()
    }

    pub fn decode(b: &[u8]) -> Option<Session> {
        util::decode(b, binary::dec_session).ok()
    }
}

// Session State ////////////////////////////////////////////////////////////

#[derive(Clone)]
pub struct SessionState {
    pub recv_chains:     RingBuf<RecvChain>,
    pub send_chain:      SendChain,
    pub root_key:        RootKey,
    pub prev_counter:    Counter,
    pub skipped_msgkeys: RingBuf<MessageKeys>
}

impl SessionState {
    fn init_as_alice(p: AliceParams) -> SessionState {
        let master_key = {
            let mut buf = Vec::new();
            buf.push_all(&p.alice_ident.secret_key.shared_secret(&p.bob.public_key));
            buf.push_all(&p.alice_base.secret_key.shared_secret(&p.bob.identity_key.public_key));
            buf.push_all(&p.alice_base.secret_key.shared_secret(&p.bob.public_key));
            buf
        };

        let dsecs = DerivedSecrets::kdf_without_salt(Input(master_key.as_slice()), Info(b"handshake"));

        // receiving chain
        let rootkey  = RootKey::from_cipher_key(dsecs.cipher_key);
        let chainkey = ChainKey::from_mac_key(dsecs.mac_key, Counter::zero());

        let mut recv_chains = RingBuf::with_capacity(MAX_RECV_CHAINS + 1);
        recv_chains.push_front(RecvChain::new(chainkey, p.bob.public_key));

        // sending chain
        let send_ratchet = KeyPair::new();
        let (rok, chk)   = rootkey.dh_ratchet(&send_ratchet, &p.bob.public_key);
        let send_chain   = SendChain::new(chk, send_ratchet);

        SessionState {
            recv_chains:     recv_chains,
            send_chain:      send_chain,
            root_key:        rok,
            prev_counter:    Counter::zero(),
            skipped_msgkeys: RingBuf::new()
        }
    }

    fn init_as_bob(p: BobParams) -> SessionState {
        let master_key = {
            let mut buf = Vec::new();
            buf.push_all(&p.bob_prekey.secret_key.shared_secret(&p.alice_ident.public_key));
            buf.push_all(&p.bob_ident.secret_key.shared_secret(p.alice_base));
            buf.push_all(&p.bob_prekey.secret_key.shared_secret(p.alice_base));
            buf
        };

        let dsecs = DerivedSecrets::kdf_without_salt(Input(master_key.as_slice()), Info(b"handshake"));

        // sending chain
        let rootkey    = RootKey::from_cipher_key(dsecs.cipher_key);
        let chainkey   = ChainKey::from_mac_key(dsecs.mac_key, Counter::zero());
        let send_chain = SendChain::new(chainkey, p.bob_prekey);

        SessionState {
            recv_chains:     RingBuf::with_capacity(MAX_RECV_CHAINS + 1),
            send_chain:      send_chain,
            root_key:        rootkey,
            prev_counter:    Counter::zero(),
            skipped_msgkeys: RingBuf::new()
        }
    }

    fn ratchet(&mut self, ratchet_key: PublicKey) {
        let new_ratchet = KeyPair::new();

        let (recv_root_key, recv_chain_key) =
            self.root_key.dh_ratchet(&self.send_chain.ratchet_key, &ratchet_key);

        let (send_root_key, send_chain_key) =
            recv_root_key.dh_ratchet(&new_ratchet, &ratchet_key);

        let recv_chain = RecvChain {
            chain_key:   recv_chain_key,
            ratchet_key: ratchet_key,
        };

        let send_chain = SendChain {
            chain_key:   send_chain_key,
            ratchet_key: new_ratchet
        };

        self.root_key     = send_root_key;
        self.prev_counter = self.send_chain.chain_key.idx;
        self.send_chain   = send_chain;

        self.recv_chains.push_front(recv_chain);

        if self.recv_chains.len() > MAX_RECV_CHAINS {
            self.recv_chains.pop_back();
        }
    }

    fn encrypt(&mut self, ident: IdentityKey, pending: &Option<(PreKeyId, PublicKey)>, plain: &[u8]) -> Envelope {
        let msgkeys = self.send_chain.chain_key.message_keys();

        let cmessage = CipherMessage {
            ratchet_key:  self.send_chain.ratchet_key.public_key,
            counter:      self.send_chain.chain_key.idx,
            prev_counter: self.prev_counter,
            cipher_text:  msgkeys.encrypt(plain)
        };

        let message = match *pending {
            None     => Message::Plain(cmessage),
            Some(pp) => Message::Keyed(PreKeyMessage {
                prekey_id:    pp.0,
                base_key:     pp.1,
                identity_key: ident,
                message:      cmessage
            })
        };

        self.send_chain.chain_key = self.send_chain.chain_key.next();
        Envelope::new(&msgkeys.mac_key, message)
    }

    fn decrypt<E>(&mut self, env: &Envelope, m: &CipherMessage) -> Result<Vec<u8>, DecryptError<E>> {
        let i = match self.recv_chains.iter().position(|c| c.ratchet_key == m.ratchet_key) {
            Some(i) => i,
            None    => {
                self.ratchet(m.ratchet_key);
                0
            }
        };

        match m.counter.cmp(&self.recv_chains[i].chain_key.idx) {
            Ordering::Less    => self.try_skipped_message_keys(env, m),
            Ordering::Greater => {
                let (chk, mk, mks) = try!(SessionState::stage_skipped_message_keys(m, &self.recv_chains[i]));
                if !env.verify(&mk.mac_key) {
                    return Err(DecryptError::InvalidSignature)
                }
                let plain = mk.decrypt(m.cipher_text.as_slice());
                self.recv_chains[i].chain_key = chk.next();
                self.commit_skipped_message_keys(mks);
                Ok(plain)
            }
            Ordering::Equal => {
                let mks = self.recv_chains[i].chain_key.message_keys();
                if !env.verify(&mks.mac_key) {
                    return Err(DecryptError::InvalidSignature)
                }
                let plain = mks.decrypt(m.cipher_text.as_slice());
                self.recv_chains[i].chain_key = self.recv_chains[i].chain_key.next();
                Ok(plain)
            }
        }
    }

    fn try_skipped_message_keys<E>(&mut self, env: &Envelope, mesg: &CipherMessage) -> Result<Vec<u8>, DecryptError<E>> {
        let too_old = self.skipped_msgkeys.get(0)
            .map(|k| k.counter > mesg.counter)
            .unwrap_or(false);

        if too_old {
            return Err(DecryptError::OutdatedMessage)
        }

        match self.skipped_msgkeys.iter().position(|mk| mk.counter == mesg.counter) {
            Some(i) => {
                let mk = self.skipped_msgkeys.remove(i).unwrap();
                if env.verify(&mk.mac_key) {
                    Ok(mk.decrypt(mesg.cipher_text.as_slice()))
                } else {
                    Err(DecryptError::InvalidMessage)
                }
            }
            None => Err(DecryptError::DuplicateMessage)
        }
    }

    fn stage_skipped_message_keys<E>(msg: &CipherMessage, chr: &RecvChain) -> Result<(ChainKey, MessageKeys, RingBuf<MessageKeys>), DecryptError<E>> {
        let num = (msg.counter.value() - chr.chain_key.idx.value()) as usize;

        if num > MAX_COUNTER_GAP {
            return Err(DecryptError::TooDistantFuture)
        }

        let mut buf = RingBuf::with_capacity(num);
        let mut chk = chr.chain_key.clone();

        for _ in 0 .. num {
            buf.push_back(chk.message_keys());
            chk = chk.next()
        }

        let mk = chk.message_keys();
        Ok((chk, mk, buf))
    }

    fn commit_skipped_message_keys(&mut self, mks: RingBuf<MessageKeys>) {
        assert!(mks.len() <= MAX_COUNTER_GAP);

        let excess = self.skipped_msgkeys.len() as isize
                   + mks.len() as isize
                   - MAX_COUNTER_GAP as isize;

        for _ in 0 .. excess {
            self.skipped_msgkeys.pop_front();
        }

        for m in mks.into_iter() {
            self.skipped_msgkeys.push_back(m)
        }

        assert!(self.skipped_msgkeys.len() <= MAX_COUNTER_GAP);
    }
}

// Decrypt Error ////////////////////////////////////////////////////////////

#[derive(Copy, PartialEq, Eq)]
pub enum DecryptError<E> {
    RemoteIdentityChanged,
    InvalidSignature,
    InvalidMessage,
    DuplicateMessage,
    TooDistantFuture,
    OutdatedMessage,
    PreKeyStoreError(E)
}

impl<E> DecryptError<E> {
    fn as_str(&self) -> &str {
        match *self {
            DecryptError::RemoteIdentityChanged => "RemoteIdentityChanged",
            DecryptError::InvalidSignature      => "InvalidSignature",
            DecryptError::InvalidMessage        => "InvalidMessage",
            DecryptError::DuplicateMessage      => "DuplicateMessage",
            DecryptError::TooDistantFuture      => "TooDistantFuture",
            DecryptError::OutdatedMessage       => "OutdatedMessage",
            DecryptError::PreKeyStoreError(_)   => "PreKeyStoreError"
        }
    }
}

impl<E: fmt::Debug> fmt::Debug for DecryptError<E> {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        match *self {
            DecryptError::PreKeyStoreError(ref e) => write!(f, "PrekeyStoreError: {:?}", e),
            _                                     => f.write_str(self.as_str())
        }
    }
}

impl<E: fmt::Display> fmt::Display for DecryptError<E> {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        match *self {
            DecryptError::PreKeyStoreError(ref e) => write!(f, "PrekeyStoreError: {}", e),
            _                                     => f.write_str(self.as_str())
        }
    }
}

impl<E: Error> Error for DecryptError<E> {
    fn description(&self) -> &str {
        self.as_str()
    }

    fn cause(&self) -> Option<&Error> {
        match *self {
            DecryptError::PreKeyStoreError(ref e) => Some(e),
            _                                     => None
        }
    }
}

impl<E: Error> FromError<E> for DecryptError<E> {
    fn from_error(err: E) -> DecryptError<E> {
        DecryptError::PreKeyStoreError(err)
    }
}

// Tests ////////////////////////////////////////////////////////////////////

#[cfg(test)]
mod tests {
    use internal::keys::{IdentityKeyPair, PreKey, PreKeyId, PreKeyBundle};
    use internal::keys::gen_prekeys;
    use internal::message::Envelope;
    use std::error::Error;
    use std::old_io::{IoResult, IoError};
    use std::vec::Vec;
    use super::*;

    struct TestStore {
        prekeys: Vec<PreKey>
    }

    impl TestStore {
        pub fn prekey_slice(&self) -> &[PreKey] {
            self.prekeys.as_slice()
        }
    }

    impl PreKeyStore<IoError> for TestStore {
        fn prekey(&self, id: PreKeyId) -> IoResult<Option<PreKey>> {
            Ok(self.prekeys.iter().find(|k| k.key_id == id).map(|k| k.clone()))
        }

        fn remove(&mut self, id: PreKeyId) -> IoResult<()> {
            self.prekeys.iter()
                .position(|k| k.key_id == id)
                .map(|ix| self.prekeys.swap_remove(ix));
            Ok(())
        }
    }

    #[test]
    fn encrypt_decrypt() {
        let alice_ident = IdentityKeyPair::new();
        let bob_ident   = IdentityKeyPair::new();

        let mut alice_store = TestStore { prekeys: gen_prekeys(PreKeyId::new(0), 10) };
        let mut bob_store   = TestStore { prekeys: gen_prekeys(PreKeyId::new(0), 10) };

        let bob_prekey = bob_store.prekey_slice().first().unwrap().clone();
        let bob_bundle = PreKeyBundle::new(bob_ident.public_key, &bob_prekey);

        let mut alice = Session::init_from_prekey(&alice_ident, bob_bundle);
        alice = Session::decode(&alice.encode()).unwrap();
        assert_eq!(1, alice.session_states[0].recv_chains.len());

        let hello_bob = alice.encrypt(b"Hello Bob!");
        let hello_bob_delayed = alice.encrypt(b"Hello delay!");
        assert_eq!(1, alice.session_states.len());
        assert_eq!(1, alice.session_states[0].recv_chains.len());

        let mut bob = assert_init_from_message(&bob_ident, &mut bob_store, &hello_bob, b"Hello Bob!");
        bob = Session::decode(&bob.encode()).unwrap();
        assert_eq!(1, bob.session_states.len());
        assert_eq!(1, bob.session_states[0].recv_chains.len());

        let hello_alice = bob.encrypt(b"Hello Alice!");

        // Alice
        assert_decrypt(b"Hello Alice!", alice.decrypt(&mut alice_store, &hello_alice));
        assert_eq!(2, alice.session_states[0].recv_chains.len());
        let ping_bob_1 = alice.encrypt(b"Ping1!");
        let ping_bob_2 = alice.encrypt(b"Ping2!");
        assert_prev_count(&alice, 2);

        // Bob
        assert_decrypt(b"Ping1!", bob.decrypt(&mut bob_store, &ping_bob_1));
        assert_eq!(2, bob.session_states[0].recv_chains.len());
        assert_decrypt(b"Ping2!", bob.decrypt(&mut bob_store, &ping_bob_2));
        assert_eq!(2, bob.session_states[0].recv_chains.len());
        let pong_alice = bob.encrypt(b"Pong!");
        assert_prev_count(&bob, 1);

        // Alice
        assert_decrypt(b"Pong!", alice.decrypt(&mut alice_store, &pong_alice));
        assert_eq!(3, alice.session_states[0].recv_chains.len());
        assert_prev_count(&alice, 2);

        // Bob (Delayed (prekey) message, decrypted with the "old" receive chain)
        assert_decrypt(b"Hello delay!", bob.decrypt(&mut bob_store, &hello_bob_delayed));
        assert_eq!(2, bob.session_states[0].recv_chains.len());
        assert_prev_count(&bob, 1);
    }

    #[test]
    fn counter_mismatch() {
        let alice_ident = IdentityKeyPair::new();
        let bob_ident   = IdentityKeyPair::new();

        let mut alice_store = TestStore { prekeys: gen_prekeys(PreKeyId::new(0), 10) };
        let mut bob_store   = TestStore { prekeys: gen_prekeys(PreKeyId::new(0), 10) };

        let bob_prekey = bob_store.prekey_slice().first().unwrap().clone();
        let bob_bundle = PreKeyBundle::new(bob_ident.public_key, &bob_prekey);

        let mut alice = Session::init_from_prekey(&alice_ident, bob_bundle);
        let hello_bob = alice.encrypt(b"Hello Bob!");

        let mut bob = assert_init_from_message(&bob_ident, &mut bob_store, &hello_bob, b"Hello Bob!");

        let hello1 = bob.encrypt(b"Hello1");
        let hello2 = bob.encrypt(b"Hello2");
        let hello3 = bob.encrypt(b"Hello3");
        let hello4 = bob.encrypt(b"Hello4");
        let hello5 = bob.encrypt(b"Hello5");

        assert_decrypt(b"Hello2", alice.decrypt(&mut alice_store, &hello2));
        assert_eq!(1, alice.session_states[0].skipped_msgkeys.len());

        assert_decrypt(b"Hello1", alice.decrypt(&mut alice_store, &hello1));
        assert_eq!(0, alice.session_states[0].skipped_msgkeys.len());

        assert_decrypt(b"Hello3", alice.decrypt(&mut alice_store, &hello3));
        assert_eq!(0, alice.session_states[0].skipped_msgkeys.len());

        assert_decrypt(b"Hello5", alice.decrypt(&mut alice_store, &hello5));
        assert_eq!(1, alice.session_states[0].skipped_msgkeys.len());

        assert_decrypt(b"Hello4", alice.decrypt(&mut alice_store, &hello4));
        assert_eq!(0, alice.session_states[0].skipped_msgkeys.len());

        for m in vec![hello1, hello2, hello3, hello4, hello5].iter() {
            assert_eq!(Some(DecryptError::DuplicateMessage), alice.decrypt(&mut alice_store, m).err());
        }
    }

    #[test]
    fn multiple_prekey_msgs() {
        let alice_ident = IdentityKeyPair::new();
        let bob_ident   = IdentityKeyPair::new();

        let mut bob_store = TestStore { prekeys: gen_prekeys(PreKeyId::new(0), 10) };

        let bob_prekey = bob_store.prekey_slice().first().unwrap().clone();
        let bob_bundle = PreKeyBundle::new(bob_ident.public_key, &bob_prekey);

        let mut alice  = Session::init_from_prekey(&alice_ident, bob_bundle);
        let hello_bob1 = alice.encrypt(b"Hello Bob1!");
        let hello_bob2 = alice.encrypt(b"Hello Bob2!");
        let hello_bob3 = alice.encrypt(b"Hello Bob3!");

        let mut bob = assert_init_from_message(&bob_ident, &mut bob_store, &hello_bob1, b"Hello Bob1!");
        assert_eq!(1, bob.session_states.len());
        assert_decrypt(b"Hello Bob2!", bob.decrypt(&mut bob_store, &hello_bob2));
        assert_eq!(1, bob.session_states.len());
        assert_decrypt(b"Hello Bob3!", bob.decrypt(&mut bob_store, &hello_bob3));
        assert_eq!(1, bob.session_states.len());
    }

    #[test]
    fn simultaneous_prekey_msgs() {
        let alice_ident = IdentityKeyPair::new();
        let bob_ident   = IdentityKeyPair::new();

        let mut alice_store = TestStore { prekeys: gen_prekeys(PreKeyId::new(0), 10) };
        let mut bob_store   = TestStore { prekeys: gen_prekeys(PreKeyId::new(0), 10) };

        let bob_prekey = bob_store.prekey_slice().first().unwrap().clone();
        let bob_bundle = PreKeyBundle::new(bob_ident.public_key, &bob_prekey);

        let alice_prekey = alice_store.prekey_slice().first().unwrap().clone();
        let alice_bundle = PreKeyBundle::new(alice_ident.public_key, &alice_prekey);

        // Initial simultaneous prekey message
        let mut alice = Session::init_from_prekey(&alice_ident, bob_bundle);
        let hello_bob = alice.encrypt(b"Hello Bob!");

        let mut bob     = Session::init_from_prekey(&bob_ident, alice_bundle);
        let hello_alice = bob.encrypt(b"Hello Alice!");

        assert_decrypt(b"Hello Bob!", bob.decrypt(&mut bob_store, &hello_bob));
        assert_eq!(2, bob.session_states.len());

        assert_decrypt(b"Hello Alice!", alice.decrypt(&mut alice_store, &hello_alice));
        assert_eq!(2, alice.session_states.len());

        // Non-simultaneous answer, which results in agreement of a session.
        let greet_bob = alice.encrypt(b"That was fast!");
        assert_decrypt(b"That was fast!", bob.decrypt(&mut bob_store, &greet_bob));

        let answer_alice = bob.encrypt(b":-)");
        assert_decrypt(b":-)", alice.decrypt(&mut alice_store, &answer_alice));
    }

    #[test]
    fn simultaneous_msgs_repeated() {
        let alice_ident = IdentityKeyPair::new();
        let bob_ident   = IdentityKeyPair::new();

        let mut alice_store = TestStore { prekeys: gen_prekeys(PreKeyId::new(0), 10) };
        let mut bob_store   = TestStore { prekeys: gen_prekeys(PreKeyId::new(0), 10) };

        let bob_prekey = bob_store.prekey_slice().first().unwrap().clone();
        let bob_bundle = PreKeyBundle::new(bob_ident.public_key, &bob_prekey);

        let alice_prekey = alice_store.prekey_slice().first().unwrap().clone();
        let alice_bundle = PreKeyBundle::new(alice_ident.public_key, &alice_prekey);

        // Initial simultaneous prekey message
        let mut alice = Session::init_from_prekey(&alice_ident, bob_bundle);
        let hello_bob = alice.encrypt(b"Hello Bob!");

        let mut bob     = Session::init_from_prekey(&bob_ident, alice_bundle);
        let hello_alice = bob.encrypt(b"Hello Alice!");

        assert_decrypt(b"Hello Bob!", bob.decrypt(&mut bob_store, &hello_bob));
        assert_decrypt(b"Hello Alice!", alice.decrypt(&mut alice_store, &hello_alice));

        // Second simultaneous message
        let echo_bob1   = alice.encrypt(b"Echo Bob1!");
        let echo_alice1 = bob.encrypt(b"Echo Alice1!");

        assert_decrypt(b"Echo Bob1!", bob.decrypt(&mut bob_store, &echo_bob1));
        assert_eq!(2, bob.session_states.len());

        assert_decrypt(b"Echo Alice1!", alice.decrypt(&mut alice_store, &echo_alice1));
        assert_eq!(2, alice.session_states.len());

        // Third simultaneous message
        let echo_bob2   = alice.encrypt(b"Echo Bob2!");
        let echo_alice2 = bob.encrypt(b"Echo Alice2!");

        assert_decrypt(b"Echo Bob2!", bob.decrypt(&mut bob_store, &echo_bob2));
        assert_eq!(2, bob.session_states.len());

        assert_decrypt(b"Echo Alice2!", alice.decrypt(&mut alice_store, &echo_alice2));
        assert_eq!(2, alice.session_states.len());

        // Non-simultaneous answer, which results in agreement of a session.
        let stop_bob = alice.encrypt(b"Stop it!");
        assert_decrypt(b"Stop it!", bob.decrypt(&mut bob_store, &stop_bob));

        let answer_alice = bob.encrypt(b"OK");
        assert_decrypt(b"OK", alice.decrypt(&mut alice_store, &answer_alice));
    }

    #[test]
    fn enc_dec_session() {
        let alice_ident = IdentityKeyPair::new();
        let bob_ident   = IdentityKeyPair::new();

        let bob_store = TestStore { prekeys: gen_prekeys(PreKeyId::new(0), 10) };

        let bob_prekey = bob_store.prekey_slice().first().unwrap().clone();
        let bob_bundle = PreKeyBundle::new(bob_ident.public_key, &bob_prekey);

        let alice = Session::init_from_prekey(&alice_ident, bob_bundle);
        let bytes = alice.encode();

        match Session::decode(bytes.as_slice()) {
            None                => panic!("Failed to decode session"),
            Some(s@Session{..}) => assert_eq!(bytes, s.encode())
        };
    }

    #[test]
    fn mass_communication() {
        let alice_ident = IdentityKeyPair::new();
        let bob_ident   = IdentityKeyPair::new();

        let mut alice_store = TestStore { prekeys: gen_prekeys(PreKeyId::new(0), 10) };
        let mut bob_store   = TestStore { prekeys: gen_prekeys(PreKeyId::new(0), 10) };

        let bob_prekey = bob_store.prekey_slice().first().unwrap().clone();
        let bob_bundle = PreKeyBundle::new(bob_ident.public_key, &bob_prekey);

        let mut alice = Session::init_from_prekey(&alice_ident, bob_bundle);
        let hello_bob = alice.encrypt(b"Hello Bob!");

        let mut bob = assert_init_from_message(&bob_ident, &mut bob_store, &hello_bob, b"Hello Bob!");

        let mut buffer = Vec::with_capacity(1000);
        for _ in 0 .. 1000 {
            buffer.push(bob.encrypt(b"Hello Alice!").encode())
        }

        for msg in buffer.iter() {
            assert_decrypt(b"Hello Alice!", alice.decrypt(&mut alice_store, &Envelope::decode(msg).unwrap()));
        }
    }

    fn assert_decrypt<E: Error>(expected: &[u8], actual: Result<Vec<u8>, DecryptError<E>>) {
        match actual {
            Ok(b)  => assert_eq!(expected, b.as_slice()),
            Err(e) => assert!(false, format!("{}", e))
        }
    }

    fn assert_init_from_message<'r, E: Error>(i: &'r IdentityKeyPair, s: &mut PreKeyStore<E>, m: &Envelope, t: &[u8]) -> Session {
        match Session::init_from_message(i, s, m) {
            Ok((s, b)) => { assert_eq!(t, b.as_slice()); s },
            Err(e)     => {
                assert!(false, format!("{}", e));
                unreachable!()
            }
        }
    }

    fn assert_prev_count(s: &Session, expected: u32) {
        assert_eq!(expected, s.session_states[0].prev_counter.value());
    }
}