//! Optimistic header and body syncing.
//!
//! This state machine builds, from a set of sources, a fully verified chain of blocks headers
//! and bodies.
//!
//! In addition to managing the sources, using [`OptimisticFullSync`] also requires holding the
//! storage of the latest finalized block.

// TODO: document better

use super::super::{blocks_tree, chain_information};
use super::optimistic;
use crate::{executor, verify::babe};

use alloc::{collections::BTreeMap, vec};
use core::{iter, num::NonZeroU32};
use hashbrown::HashSet;

pub use optimistic::{
    FinishRequestOutcome, RequestAction, RequestFail, RequestId, SourceId, Start,
};

/// Configuration for the [`OptimisticFullSync`].
#[derive(Debug)]
pub struct Config {
    /// Information about the latest finalized block and its ancestors.
    pub chain_information: chain_information::ChainInformation,

    /// Configuration for BABE, retreived from the genesis block.
    pub babe_genesis_config: babe::BabeGenesisConfiguration,

    /// Pre-allocated capacity for the number of block sources.
    pub sources_capacity: usize,

    /// Pre-allocated capacity for the number of blocks between the finalized block and the head
    /// of the chain.
    ///
    /// Should be set to the maximum number of block between two consecutive justifications.
    pub blocks_capacity: usize,

    /// Maximum number of blocks returned by a response.
    ///
    /// > **Note**: If blocks are requested from the network, this should match the network
    /// >           protocol enforced limit.
    pub blocks_request_granularity: NonZeroU32,

    /// Number of blocks to download ahead of the best block.
    ///
    /// Whenever the latest best block is updated, the state machine will start block
    /// requests for the block `best_block_height + download_ahead_blocks` and all its
    /// ancestors. Considering that requesting blocks has some latency, downloading blocks ahead
    /// of time ensures that verification isn't blocked waiting for a request to be finished.
    ///
    /// The ideal value here depends on the speed of blocks verification speed and latency of
    /// block requests.
    pub download_ahead_blocks: u32,

    /// Seed used by the PRNG (Pseudo-Random Number Generator) that selects which source to start
    /// requests with.
    ///
    /// You are encouraged to use something like `rand::random()` to fill this field, except in
    /// situations where determinism/reproducibility is desired.
    pub source_selection_randomness_seed: u64,
}

/// Optimistic headers-only syncing.
pub struct OptimisticFullSync<TRq, TSrc> {
    // TODO: doc
    chain: blocks_tree::NonFinalizedTree<()>,

    /// Changes in the storage of the best block compared to the finalized block.
    /// The `BTreeMap`'s keys are storage keys, and its values are new values or `None` if the
    /// value has been erased from the storage.
    best_to_finalized_storage_diff: BTreeMap<Vec<u8>, Option<Vec<u8>>>,

    /// Compiled runtime code of the best block block.
    /// This field is a cache. As such, it will stay at `None` until this value has been needed
    /// for the first time.
    runtime_code_cache: Option<executor::WasmVmPrototype>,

    /// Underlying helper. Manages sources and requests.
    /// Always `Some`, except during some temporary extractions.
    sync: Option<optimistic::OptimisticSync<TRq, TSrc, RequestSuccessBlock>>,
}

impl<TRq, TSrc> OptimisticFullSync<TRq, TSrc> {
    /// Builds a new [`OptimisticFullSync`].
    pub fn new(config: Config) -> Self {
        let chain = blocks_tree::NonFinalizedTree::new(blocks_tree::Config {
            chain_information: config.chain_information.clone(),
            babe_genesis_config: config.babe_genesis_config,
            blocks_capacity: config.blocks_capacity,
        });

        let best_block_number = chain.best_block_header().number;

        OptimisticFullSync {
            chain,
            best_to_finalized_storage_diff: BTreeMap::new(),
            runtime_code_cache: None,
            sync: Some(optimistic::OptimisticSync::new(optimistic::Config {
                best_block_number,
                sources_capacity: config.sources_capacity,
                blocks_request_granularity: config.blocks_request_granularity,
                download_ahead_blocks: config.download_ahead_blocks,
                source_selection_randomness_seed: config.source_selection_randomness_seed,
            })),
        }
    }

    /// Builds a [`chain_information::ChainInformationRef`] struct corresponding to the current
    /// latest finalized block. Can later be used to reconstruct a chain.
    pub fn as_chain_information(&self) -> chain_information::ChainInformationRef {
        self.chain.as_chain_information()
    }

    /// Returns the number of the best block.
    ///
    /// > **Note**: This value is provided only for informative purposes. Keep in mind that this
    /// >           best block might be reverted in the future.
    pub fn best_block_number(&self) -> u64 {
        self.chain.best_block_header().number
    }

    /// Returns the hash of the best block.
    ///
    /// > **Note**: This value is provided only for informative purposes. Keep in mind that this
    /// >           best block might be reverted in the future.
    pub fn best_block_hash(&self) -> [u8; 32] {
        self.chain.best_block_hash()
    }

    /// Inform the [`OptimisticFullSync`] of a new potential source of blocks.
    pub fn add_source(&mut self, source: TSrc) -> SourceId {
        self.sync.as_mut().unwrap().add_source(source)
    }

    /// Inform the [`OptimisticFullSync`] that a source of blocks is no longer available.
    ///
    /// This automatically cancels all the requests that have been emitted for this source.
    /// This list of requests is returned as part of this function.
    ///
    /// # Panic
    ///
    /// Panics if the [`SourceId`] is invalid.
    ///
    pub fn remove_source<'a>(
        &'a mut self,
        source: SourceId,
    ) -> (TSrc, impl Iterator<Item = (RequestId, TRq)> + 'a) {
        self.sync.as_mut().unwrap().remove_source(source)
    }

    /// Returns an iterator that extracts all requests that need to be started and requests that
    /// need to be cancelled.
    pub fn next_request_action(&mut self) -> Option<RequestAction<TRq, TSrc, RequestSuccessBlock>> {
        self.sync.as_mut().unwrap().next_request_action()
    }

    /// Update the [`OptimisticFullSync`] with the outcome of a request.
    ///
    /// Returns the user data that was associated to that request.
    ///
    /// # Panic
    ///
    /// Panics if the [`RequestId`] is invalid.
    ///
    pub fn finish_request<'a>(
        &'a mut self,
        request_id: RequestId,
        outcome: Result<impl Iterator<Item = RequestSuccessBlock>, RequestFail>,
    ) -> (TRq, FinishRequestOutcome<'a, TSrc>) {
        self.sync
            .as_mut()
            .unwrap()
            .finish_request(request_id, outcome)
    }

    /// Process a chunk of blocks in the queue of verification.
    ///
    /// This method takes ownership of the [`OptimisticFullSync`] and starts a verification
    /// process. The [`OptimisticFullSync`] is yielded back at the end of this process.
    pub fn process_one(mut self) -> ProcessOne<TRq, TSrc> {
        let sync = self.sync.take().unwrap();

        let to_process = match sync.process_one() {
            Ok(tp) => tp,
            Err(sync) => {
                self.sync = Some(sync);
                return ProcessOne::Finished { sync: self };
            }
        };

        self.chain.reserve(to_process.blocks.len());

        ProcessOne::from(
            Inner::Start(self.chain),
            ProcessOneShared {
                to_process,
                best_to_finalized_storage_diff: self.best_to_finalized_storage_diff,
                runtime_code_cache: self.runtime_code_cache,
            },
        )
    }
}

pub struct RequestSuccessBlock {
    pub scale_encoded_header: Vec<u8>,
    pub scale_encoded_justification: Option<Vec<u8>>,
    pub scale_encoded_extrinsics: Vec<Vec<u8>>,
}

/// State of the processing of blocks.
pub enum ProcessOne<TRq, TSrc> {
    /// Processing is over.
    Finished {
        /// The state machine.
        /// The [`OptimisticFullSync::process_one`] method takes ownership of the
        /// [`OptimisticFullSync`]. This field yields it back.
        sync: OptimisticFullSync<TRq, TSrc>,
        // TODO: finalized_advance: ,
    },
    /// Loading a storage value of the finalized block is required in order to continue.
    FinalizedStorageGet(StorageGet<TRq, TSrc>),
    /// Fetching the list of keys of the finalized block with a given prefix is required in order
    /// to continue.
    FinalizedStoragePrefixKeys(StoragePrefixKeys<TRq, TSrc>),
    /// Fetching the key of the finalized block storage that follows a given one is required in
    /// order to continue.
    FinalizedStorageNextKey(StorageNextKey<TRq, TSrc>),
}

enum Inner {
    Start(blocks_tree::NonFinalizedTree<()>),
    Step1(blocks_tree::BodyVerifyStep1<(), vec::IntoIter<Vec<u8>>>),
    Step2(blocks_tree::BodyVerifyStep2<()>),
}

struct ProcessOneShared<TRq, TSrc> {
    to_process: optimistic::ProcessOne<TRq, TSrc, RequestSuccessBlock>,
    best_to_finalized_storage_diff: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    runtime_code_cache: Option<executor::WasmVmPrototype>,
}

impl<TRq, TSrc> ProcessOne<TRq, TSrc> {
    fn from(mut inner: Inner, mut shared: ProcessOneShared<TRq, TSrc>) -> Self {
        // This loop drives the process of the verification.
        // `inner` is updated at each iteration until a state that cannot be resolved internally
        // is found.
        'verif_steps: loop {
            match inner {
                Inner::Start(chain) => {
                    // TODO: verify justification

                    if !shared.to_process.blocks.as_slice().is_empty() {
                        let next_block = shared.to_process.blocks.next().unwrap();
                        inner = Inner::Step1(chain.verify_body(
                            next_block.scale_encoded_header,
                            next_block.scale_encoded_extrinsics.into_iter(),
                        ));
                    } else {
                        debug_assert!(shared.to_process.blocks.as_slice().is_empty());
                        let sync = shared
                            .to_process
                            .report
                            .update_block_height(chain.best_block_header().number);
                        break ProcessOne::Finished {
                            sync: OptimisticFullSync {
                                chain,
                                best_to_finalized_storage_diff: shared
                                    .best_to_finalized_storage_diff,
                                runtime_code_cache: shared.runtime_code_cache,
                                sync: Some(sync),
                            },
                        };
                    }
                }

                Inner::Step1(blocks_tree::BodyVerifyStep1::InvalidHeader(chain, error)) => {
                    // TODO: DRY
                    let sync = shared
                        .to_process
                        .report
                        .reset_to_finalized(chain.finalized_block_header().number);
                    break ProcessOne::Finished {
                        sync: OptimisticFullSync {
                            chain,
                            best_to_finalized_storage_diff: Default::default(),
                            runtime_code_cache: None,
                            sync: Some(sync),
                        },
                    };
                }

                Inner::Step1(blocks_tree::BodyVerifyStep1::Duplicate(chain)) => {
                    // TODO: DRY
                    let sync = shared
                        .to_process
                        .report
                        .reset_to_finalized(chain.finalized_block_header().number);
                    break ProcessOne::Finished {
                        sync: OptimisticFullSync {
                            chain,
                            best_to_finalized_storage_diff: Default::default(),
                            runtime_code_cache: None,
                            sync: Some(sync),
                        },
                    };
                }

                Inner::Step1(blocks_tree::BodyVerifyStep1::BadParent { chain, .. }) => {
                    // TODO: DRY
                    let sync = shared
                        .to_process
                        .report
                        .reset_to_finalized(chain.finalized_block_header().number);
                    break ProcessOne::Finished {
                        sync: OptimisticFullSync {
                            chain,
                            best_to_finalized_storage_diff: shared.best_to_finalized_storage_diff,
                            runtime_code_cache: shared.runtime_code_cache,
                            sync: Some(sync),
                        },
                    };
                }

                Inner::Step1(blocks_tree::BodyVerifyStep1::ParentRuntimeRequired(req)) => {
                    // The verification process is asking for a Wasm virtual machine containing
                    // the parent block's runtime.
                    //
                    // Since virtual machines are expensive to create, a re-usable virtual machine
                    // is maintained for the best block.
                    //
                    // The code below extracts that re-usable virtual machine with the intention
                    // to store it back after the verification is over.
                    let parent_runtime = match shared.runtime_code_cache.take() {
                        Some(r) => r,
                        None => {
                            if let Some(code) =
                                shared.best_to_finalized_storage_diff.get(&b":code"[..])
                            {
                                let code = code.as_ref().expect("no runtime code?!?!"); // TODO: what to do?
                                executor::WasmVmPrototype::new(&code)
                                    .expect("invalid runtime code?!?!") // TODO: what to do?
                            } else {
                                // No cache has been found anywhere in the hierarchy.
                                // The user needs to be asked for the storage entry containing the
                                // runtime code.
                                return ProcessOne::FinalizedStorageGet(StorageGet {
                                    inner: StorageGetTarget::Runtime(req),
                                    shared,
                                });
                            }
                        }
                    };

                    inner = Inner::Step2(req.resume(parent_runtime));
                }

                Inner::Step2(blocks_tree::BodyVerifyStep2::Finished {
                    storage_top_trie_changes,
                    parent_runtime,
                    result: Ok(success),
                }) => {
                    let sync = success.insert(());

                    if !storage_top_trie_changes.contains_key(&b":code"[..]) {
                        shared.runtime_code_cache = Some(parent_runtime);
                    }

                    for (key, value) in storage_top_trie_changes {
                        shared.best_to_finalized_storage_diff.insert(key, value);
                    }

                    // TODO: remove
                    let n = sync.best_block_header().number;
                    println!("now at {:?}", n);

                    inner = Inner::Start(sync);
                }

                Inner::Step2(blocks_tree::BodyVerifyStep2::Finished {
                    result: Err(err), ..
                }) => todo!("verif failure"),

                Inner::Step2(blocks_tree::BodyVerifyStep2::StorageGet(mut req)) => {
                    // The underlying verification process is asking for a storage entry in the
                    // parent block.
                    //
                    // The [`OptimisticFullSync`] stores the difference between the best block's
                    // storage and the finalized block's storage.
                    // As such, the requested value is either found in one of this diff, in which
                    // case it can be returned immediately to continue the verification, or in
                    // the finalized block, in which case the user needs to be queried.
                    // TODO: a bit stupid to have to allocate for the key
                    let key = req.key().fold(Vec::new(), |mut a, b| {
                        a.extend_from_slice(b.as_ref());
                        a
                    });

                    if let Some(value) = shared.best_to_finalized_storage_diff.get(&key) {
                        let value = value.clone(); // TODO: necessary for borrowing issues :(
                        inner = Inner::Step2(req.inject_value(value.as_ref().map(|v| &v[..])));
                        continue 'verif_steps;
                    }

                    // The value hasn't been found in any of the diffs, meaning that the storage
                    // value of the parent is the same as the one of the finalized block. The
                    // user needs to be queried.
                    break ProcessOne::FinalizedStorageGet(StorageGet {
                        inner: StorageGetTarget::Storage(req),
                        shared,
                    });
                }

                Inner::Step2(blocks_tree::BodyVerifyStep2::StorageNextKey(req)) => {
                    // The underlying verification process is asking for the key that follows
                    // the requested one.

                    // TODO: no; must look through hierarchy
                    break ProcessOne::FinalizedStorageNextKey(StorageNextKey {
                        inner: req,
                        shared,
                    });
                }

                Inner::Step2(blocks_tree::BodyVerifyStep2::StoragePrefixKeys(req)) => {
                    // The underlying verification process is asking for all the keys that start
                    // with a certain prefix.
                    // The first step is to ask the user for that information when it comes to
                    // the finalized block.
                    break ProcessOne::FinalizedStoragePrefixKeys(StoragePrefixKeys {
                        inner: req,
                        shared,
                    });
                }
            }
        }
    }
}

/// Loading a storage value is required in order to continue.
#[must_use]
pub struct StorageGet<TRq, TBl> {
    inner: StorageGetTarget,
    shared: ProcessOneShared<TRq, TBl>,
}

enum StorageGetTarget {
    Storage(blocks_tree::StorageGet<()>),
    Runtime(blocks_tree::BodyVerifyRuntimeRequired<(), vec::IntoIter<Vec<u8>>>),
}

impl<TRq, TBl> StorageGet<TRq, TBl> {
    /// Returns the key whose value must be passed to [`StorageGet::inject_value`].
    // TODO: shouldn't be mut
    pub fn key<'b>(&'b mut self) -> impl Iterator<Item = impl AsRef<[u8]> + 'b> + 'b {
        match &mut self.inner {
            StorageGetTarget::Storage(inner) => {
                either::Either::Left(inner.key().map(either::Either::Left))
            }
            StorageGetTarget::Runtime(_) => {
                either::Either::Right(iter::once(either::Either::Right(b":code")))
            }
        }
    }

    /// Injects the corresponding storage value.
    // TODO: change API, see unsealed::StorageGet
    pub fn inject_value(self, value: Option<&[u8]>) -> ProcessOne<TRq, TBl> {
        match self.inner {
            StorageGetTarget::Storage(inner) => {
                let inner = inner.inject_value(value);
                ProcessOne::from(Inner::Step2(inner), self.shared)
            }
            StorageGetTarget::Runtime(inner) => {
                let wasm_code = value.expect("no runtime code in storage?"); // TODO: ?!?!
                let wasm_vm =
                    executor::WasmVmPrototype::new(wasm_code).expect("invalid runtime code?!?!"); // TODO: ?!?!
                let inner = inner.resume(wasm_vm);
                ProcessOne::from(Inner::Step2(inner), self.shared)
            }
        }
    }
}

/// Fetching the list of keys with a given prefix is required in order to continue.
#[must_use]
pub struct StoragePrefixKeys<TRq, TBl> {
    inner: blocks_tree::StoragePrefixKeys<()>,
    shared: ProcessOneShared<TRq, TBl>,
}

impl<TRq, TBl> StoragePrefixKeys<TRq, TBl> {
    /// Returns the prefix whose keys to load.
    // TODO: don't take &mut self but &self
    pub fn prefix(&mut self) -> &[u8] {
        self.inner.prefix()
    }

    /// Injects the list of keys.
    pub fn inject_keys(
        mut self,
        keys: impl Iterator<Item = impl AsRef<[u8]>>,
    ) -> ProcessOne<TRq, TBl> {
        let mut keys = keys
            .map(|k| k.as_ref().to_owned())
            .collect::<HashSet<_, fnv::FnvBuildHasher>>();

        let prefix = self.inner.prefix().to_owned(); // TODO: meh
        for (k, v) in self
            .shared
            .best_to_finalized_storage_diff
            .range(prefix.clone()..)
            .take_while(|(k, _)| k.starts_with(&prefix))
        {
            if v.is_some() {
                keys.insert(k.clone());
            } else {
                keys.remove(k);
            }
        }

        let inner = self.inner.inject_keys(keys.iter());
        ProcessOne::from(Inner::Step2(inner), self.shared)
    }
}

/// Fetching the key that follows a given one is required in order to continue.
#[must_use]
pub struct StorageNextKey<TRq, TBl> {
    inner: blocks_tree::StorageNextKey<()>,
    shared: ProcessOneShared<TRq, TBl>,
}

impl<TRq, TBl> StorageNextKey<TRq, TBl> {
    /// Returns the key whose next key must be passed back.
    // TODO: don't take &mut self but &self
    pub fn key(&mut self) -> &[u8] {
        self.inner.key()
    }

    /// Injects the key.
    pub fn inject_key(self, key: Option<impl AsRef<[u8]>>) -> ProcessOne<TRq, TBl> {
        // TODO: finish
        let inner = self.inner.inject_key(key);
        ProcessOne::from(Inner::Step2(inner), self.shared)
    }
}