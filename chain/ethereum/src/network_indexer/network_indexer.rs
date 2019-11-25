use futures::future::{loop_fn, Loop};
use futures::sync::mpsc::{channel, Receiver, Sender};
use futures::try_ready;
use state_machine_future::*;
use std::fmt;
use std::ops::Range;
use std::str::FromStr;

use graph::prelude::*;
use web3::types::H256;

use super::block_writer::BlockWriter;
use super::*;

/**
 * Helper types.
 */

type LocalHeadFuture = Box<dyn Future<Item = Option<EthereumBlockPointer>, Error = Error> + Send>;
type ChainHeadFuture = Box<dyn Future<Item = LightEthereumBlock, Error = Error> + Send>;
type BlockFuture = Box<dyn Future<Item = Option<BlockWithUncles>, Error = Error> + Send>;
type BlockStream = Box<dyn Stream<Item = Option<BlockWithUncles>, Error = Error> + Send>;
type ForkedBlocksFuture = Box<dyn Future<Item = Vec<BlockWithUncles>, Error = Error> + Send>;
type CollectBlocksToRevertFuture =
    Box<dyn Future<Item = Vec<EthereumBlockPointer>, Error = Error> + Send>;
type RevertBlocksFuture = Box<dyn Future<Item = EthereumBlockPointer, Error = Error> + Send>;
type AddBlockFuture = Box<dyn Future<Item = EthereumBlockPointer, Error = Error> + Send>;
type SendEventFuture = Box<dyn Future<Item = (), Error = Error> + Send>;

/**
 * Helpers to create futures and streams.
 */

fn poll_chain_head(logger: Logger, adapter: Arc<dyn EthereumAdapter>) -> ChainHeadFuture {
    debug!(logger, "Poll chain head");
    Box::new(adapter.latest_block(&logger).from_err())
}

fn fetch_block_and_uncles_by_number(
    logger: Logger,
    adapter: Arc<dyn EthereumAdapter>,
    block_number: u64,
) -> BlockFuture {
    let logger_for_full_block = logger.clone();
    let adapter_for_full_block = adapter.clone();

    let logger_for_uncles = logger.clone();
    let adapter_for_uncles = adapter.clone();

    Box::new(
        adapter
            .block_by_number(&logger, block_number)
            .from_err()
            .and_then(move |block| match block {
                None => Box::new(future::ok(None))
                    as Box<dyn Future<Item = Option<EthereumBlock>, Error = _> + Send>,
                Some(block) => Box::new(
                    adapter_for_full_block
                        .load_full_block(&logger_for_full_block, block)
                        .map(|block| Some(block))
                        .from_err(),
                ),
            })
            .and_then(move |block| match block {
                None => Box::new(future::ok(None))
                    as Box<dyn Future<Item = Option<BlockWithUncles>, Error = _> + Send>,
                Some(block) => Box::new(
                    adapter_for_uncles
                        .uncles(&logger_for_uncles, &block.block)
                        .and_then(move |uncles| future::ok(BlockWithUncles { block, uncles }))
                        .map(|block| Some(block)),
                ),
            }),
    )
}

fn fetch_block_and_uncles(
    logger: Logger,
    adapter: Arc<dyn EthereumAdapter>,
    block_hash: H256,
) -> BlockFuture {
    let logger_for_full_block = logger.clone();
    let adapter_for_full_block = adapter.clone();

    let logger_for_uncles = logger.clone();
    let adapter_for_uncles = adapter.clone();

    Box::new(
        adapter
            .block_by_hash(&logger, block_hash)
            .from_err()
            .and_then(move |block| match block {
                None => Box::new(future::ok(None))
                    as Box<dyn Future<Item = Option<EthereumBlock>, Error = _> + Send>,
                Some(block) => Box::new(
                    adapter_for_full_block
                        .load_full_block(&logger_for_full_block, block)
                        .map(|block| Some(block))
                        .from_err(),
                ),
            })
            .and_then(move |block| match block {
                None => Box::new(future::ok(None))
                    as Box<dyn Future<Item = Option<BlockWithUncles>, Error = _> + Send>,
                Some(block) => Box::new(
                    adapter_for_uncles
                        .uncles(&logger_for_uncles, &block.block)
                        .and_then(move |uncles| future::ok(BlockWithUncles { block, uncles }))
                        .map(|block| Some(block)),
                ),
            }),
    )
}

fn fetch_blocks(
    logger: Logger,
    adapter: Arc<dyn EthereumAdapter>,
    block_numbers: Range<u64>,
) -> BlockStream {
    Box::new(
        futures::stream::iter_ok::<_, Error>(block_numbers)
            .map(move |block_number| {
                fetch_block_and_uncles_by_number(logger.clone(), adapter.clone(), block_number)
            })
            .buffered(100),
    )
}

fn fetch_forked_blocks(
    logger: Logger,
    subgraph_id: SubgraphDeploymentId,
    adapter: Arc<dyn EthereumAdapter>,
    store: Arc<dyn Store>,
    head: BlockWithUncles,
) -> ForkedBlocksFuture {
    // Start at `head` and go back block by block until we find a block that we
    // already have in the store. That block is the fork base. Collect all
    // blocks as we go. Then, return all blocks including the fork base and
    // head.
    Box::new(loop_fn(vec![head], move |mut blocks| {
        let store = store.clone();

        // Get the last block from the list
        let (block_entity_key, number, hash, parent_hash) = {
            let block = blocks.last().unwrap();
            (
                block.to_entity_key(subgraph_id.clone()),
                block.inner().number.clone().unwrap(),
                block.inner().hash.clone().unwrap(),
                block.inner().parent_hash.clone(),
            )
        };

        trace!(
            logger,
            "Fetch block on new chain";
            "block" => format!("{}/{:x}", number, hash),
        );

        // Look it up from the store
        match store.get(block_entity_key) {
            Ok(None) => {
                // We don't have the block yet, continue with its parent
                Box::new(
                    fetch_block_and_uncles(logger.clone(), adapter.clone(), parent_hash.clone())
                        .and_then(move |parent| match parent {
                            None => future::err(format_err!(
                                "failed to fetch parent block {:x}",
                                parent_hash
                            )),

                            Some(parent) => {
                                blocks.push(parent);
                                future::ok(Loop::Continue(blocks))
                            }
                        }),
                )
            }

            Ok(Some(_)) => {
                // We have the block already, so this is the block after which
                // the chain was forked
                Box::new(future::ok(Loop::Break(blocks)))
                    as Box<dyn Future<Item = Loop<_, _>, Error = Error> + Send>
            }

            // Looking up the block failed, propoagate the error so we can
            // retry handling the reorg
            Err(e) => Box::new(future::err(e.into()))
                as Box<dyn Future<Item = Loop<_, _>, Error = Error> + Send>,
        }
    }))
}

fn write_block(block_writer: Arc<BlockWriter>, block: BlockWithUncles) -> AddBlockFuture {
    let block_ptr = block.inner().into();
    Box::new(block_writer.write(block).map(move |_| block_ptr))
}

fn collect_blocks_to_revert(
    logger: Logger,
    subgraph_id: SubgraphDeploymentId,
    store: Arc<dyn Store>,
    head: EthereumBlockPointer,
    fork_base: EthereumBlockPointer,
) -> CollectBlocksToRevertFuture {
    trace!(
        logger,
        "Collect local blocks to revert";
        "fork_base" => format_block_pointer(&fork_base),
    );

    Box::new(loop_fn(vec![head], move |mut blocks| {
        let logger = logger.clone();
        let store = store.clone();

        // Get the last block from the list
        let block_ptr = blocks.last().unwrap().clone();
        let block_ptr_for_missing_parent = block_ptr.clone();
        let block_ptr_for_invalid_parent = block_ptr.clone();

        trace!(
            logger,
            "Collect local block to revert";
            "fork_base" => format_block_pointer(&fork_base),
            "block" => format_block_pointer(&block_ptr),
        );

        // If we've reached the fork base, terminate the loop and return
        // the blocks we have collected up to here
        if block_ptr == fork_base {
            trace!(logger, "Collect blocks complete");

            return Box::new(future::ok(Loop::Break(blocks)))
                as Box<dyn Future<Item = _, Error = _> + Send>;
        }

        // Look this block up from the store
        Box::new(
            future::result(
                store
                    .get(block_ptr.to_entity_key(subgraph_id.clone()))
                    .map_err(|e| e.into())
                    .and_then(|entity| {
                        entity.ok_or_else(|| {
                            format_err!(
                                "block missing in database: {}",
                                format_block_pointer(&block_ptr)
                            )
                        })
                    }),
            )
            // Get the parent hash from the block
            .and_then(move |block| {
                future::result(
                    block
                        .get("parent")
                        .ok_or_else(move || {
                            format_err!(
                                "block is missing a parent_hash: {}",
                                format_block_pointer(&block_ptr_for_missing_parent),
                            )
                        })
                        .and_then(|value| {
                            let s = value
                                .clone()
                                .as_string()
                                .expect("the `parent` field of `Block` is a reference/string");

                            H256::from_str(s.as_str()).map_err(|e| {
                                format_err!(
                                    "block {} has an invalid parent hash `{}`: {}",
                                    format_block_pointer(&block_ptr_for_invalid_parent),
                                    s,
                                    e,
                                )
                            })
                        }),
                )
            })
            .and_then(move |parent_hash: H256| {
                // Create a block pointer for the parent
                let parent_ptr = EthereumBlockPointer {
                    number: block_ptr.number - 1,
                    hash: parent_hash,
                };

                // Add the parent block pointer for the next iteration
                blocks.push(parent_ptr);
                future::ok(Loop::Continue(blocks))
            }),
        )
    }))
}

fn revert_blocks(
    subgraph_id: SubgraphDeploymentId,
    logger: Logger,
    store: Arc<dyn Store>,
    event_sink: Sender<NetworkIndexerEvent>,
    blocks: Vec<EthereumBlockPointer>,
) -> RevertBlocksFuture {
    let fork_base = blocks.last().expect("no blocks to revert").clone();

    debug!(logger, "Revert blocks");

    let logger_for_complete = logger.clone();

    Box::new(
        stream::iter_ok(
            blocks[0..]
                .to_owned()
                .into_iter()
                .zip(blocks[1..].to_owned().into_iter()),
        )
        .for_each(move |(from, to)| {
            let event_sink = event_sink.clone();
            let logger = logger.clone();

            debug!(
                logger,
                "Revert block";
                "from" => format_block_pointer(&from),
                "to" => format_block_pointer(&to),
            );

            future::result(store.revert_block_operations(
                subgraph_id.clone(),
                from.clone(),
                to.clone(),
            ))
            .from_err()
            .and_then(move |_| {
                send_event(
                    event_sink.clone(),
                    NetworkIndexerEvent::Revert {
                        from: from.clone(),
                        to: to.clone(),
                    },
                )
            })
        })
        .and_then(move |_| {
            debug!(
                logger_for_complete,
                "Revert blocks complete; move forward to the new chain head"
            );
            future::ok(fork_base)
        }),
    )
}

fn send_event(
    event_sink: Sender<NetworkIndexerEvent>,
    event: NetworkIndexerEvent,
) -> SendEventFuture {
    Box::new(
        event_sink
            .send(event)
            .map(|_| ())
            .map_err(|e| format_err!("failed to emit events: {}", e)),
    )
}

/**
 * Network tracer implementation.
 */

/// Context for the network tracer.
pub struct Context {
    subgraph_id: SubgraphDeploymentId,
    logger: Logger,
    adapter: Arc<dyn EthereumAdapter>,
    store: Arc<dyn Store>,
    event_sink: Sender<NetworkIndexerEvent>,
    block_writer: Arc<BlockWriter>,
}

/// Events emitted by the network tracer.
#[derive(Debug, PartialEq, Clone)]
pub enum NetworkIndexerEvent {
    Revert {
        from: EthereumBlockPointer,
        to: EthereumBlockPointer,
    },
    AddBlock(EthereumBlockPointer),
}

impl fmt::Display for NetworkIndexerEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NetworkIndexerEvent::Revert { from, to } => write!(
                f,
                "Revert: From {} to {}",
                format_block_pointer(&from),
                format_block_pointer(&to),
            ),
            NetworkIndexerEvent::AddBlock(block) => {
                write!(f, "Add block: {}", format_block_pointer(&block))
            }
        }
    }
}

/// State machine that handles block fetching and block reorganizations.
#[derive(StateMachineFuture)]
#[state_machine_future(context = "Context")]
enum StateMachine {
    /// We start with an empty state, which immediately moves on
    /// to loading the local head block of the network subgraph.
    #[state_machine_future(start, transitions(LoadLocalHead))]
    Start,

    /// This state waits until we have the local head block (we get
    /// its pointer from the store), then moves on to identifying
    /// the chain head block (the latest block on the network).
    #[state_machine_future(transitions(PollChainHead, Failed))]
    LoadLocalHead { local_head: LocalHeadFuture },

    /// This state waits until the chain head block is available.
    /// Once we have this (local head, chain head) pair, we can fetch
    /// the blocks (local head)+1, (local head)+2, ..., (chain head).
    /// We do this in smaller chunks however, for two reasons:
    ///
    /// 1. To limit the amount of blocks we keep in memory.
    /// 2. To be able to check for reorgs frequently.
    ///
    /// From this state, we move on by deciding on a range of blocks
    /// and creating a stream to pull these in with some parallelization.
    /// The next state (`ProcessBlocks`) will then read this stream block
    /// by block.
    #[state_machine_future(transitions(ProcessBlocks, PollChainHead, Failed))]
    PollChainHead {
        local_head: Option<EthereumBlockPointer>,
        chain_head: ChainHeadFuture,
    },

    /// This state takes the first block from the stream. If the stream is
    /// exhausted, it transitions back to re-checking the chain head block
    /// and deciding on the next chunk of blocks to fetch. If there is still
    /// a block to read from the stream, it's passed on to the `VetBlock`
    /// state for reorg checking.
    #[state_machine_future(transitions(VetBlock, PollChainHead, Failed))]
    ProcessBlocks {
        local_head: Option<EthereumBlockPointer>,
        chain_head: LightEthereumBlock,
        next_blocks: BlockStream,
    },

    /// This state checks whether the incoming block is the successor
    /// of the local head block. If it is, it is emitted via the `EmitEvents`
    /// state. If it is not a successor then we are dealing with a block
    /// reorg, i.e., a block that is on a fork of the chain.
    ///
    /// Note that by checking parent/child succession, this state ensures
    /// that there are no gaps. So if we are on block `x` and a block `f`
    /// comes in that is not a child, it must be on a fork of the chain,
    /// e.g.:
    ///
    ///    a---b---c---x
    ///        \
    ///         +--d---e---f
    ///
    /// In that case we need to do the following:
    ///
    /// 1. Find the fork base block (in the above example: `b`)
    /// 2. Fetch all blocks on the path between and including that
    ///    fork base block and the incoming block (in the example:
    ///    `b`, `d`, `e` and `f`)
    ///
    /// Once we have all necessary blocks (in the example: `b`, `d`, `e`
    /// and `f`), there are two actions we need to perform:
    ///
    /// a. Revert the network data to the fork base block (`b`)
    /// b. Add all blocks after the fork base block, including the
    ///    incoming block, to the indexed network data (`d`, `e` and `f`)
    ///
    /// Steps 1 and 2 are performed by identifying the incoming
    /// block as a reorg and transitioning to the `FetchForkedBlocks`
    /// state. Once that has completed the above steps, it will
    /// emit events for a) and b).
    #[state_machine_future(transitions(FetchForkedBlocks, AddBlock, PollChainHead, Failed))]
    VetBlock {
        local_head: Option<EthereumBlockPointer>,
        chain_head: LightEthereumBlock,
        next_blocks: BlockStream,
        block: BlockWithUncles,
    },

    /// Given a block identify as being on a fork of the chain, this state tries
    /// to identify the fork base block and collect all blocks on the path from
    /// the incoming block to the fork base.
    ///
    /// If successful, it moves on new_local_head to the base (`RevertToForkBase`) and
    /// then to adding the next new block with `AddBlock`. If not successful, resets
    /// to `PollChainHead` and tries again.
    ///
    /// Note: This state carries over the incoming block stream to not lose its
    /// blocks. This is because even if there was a reorg, the blocks following
    /// the current block that made us detect it will likely be valid successors.
    /// So once the reorg has been handled, we should be able to continue with
    /// the remaining blocks on the stream.
    ///
    /// Only when we reset back to `PollChainHead` do we throw away the
    /// stream in the hope that we'll get a better chain head with different
    /// blocks leading up to it.
    #[state_machine_future(transitions(RevertToForkBase, PollChainHead, Failed))]
    FetchForkedBlocks {
        local_head: Option<EthereumBlockPointer>,
        chain_head: LightEthereumBlock,
        next_blocks: BlockStream,
        forked_blocks: ForkedBlocksFuture,
    },

    #[state_machine_future(transitions(ProcessBlocks, PollChainHead, Failed))]
    RevertToForkBase {
        local_head: Option<EthereumBlockPointer>,
        chain_head: LightEthereumBlock,
        next_blocks: BlockStream,
        new_local_head: RevertBlocksFuture,
    },

    /// Waits until all events have been emitted/sent, then transition
    /// back to processing blocks from the open stream of incoming blocks.
    #[state_machine_future(transitions(ProcessBlocks, PollChainHead, Failed))]
    AddBlock {
        chain_head: LightEthereumBlock,
        next_blocks: BlockStream,
        old_local_head: Option<EthereumBlockPointer>,
        new_local_head: AddBlockFuture,
    },

    /// This is unused, the indexing never ends.
    #[state_machine_future(ready)]
    Ready(()),

    /// State for fatal errors that cause the indexing to terminate.
    /// This should almost never happen. If it does, it should cause
    /// the entire node to crash / restart.
    #[state_machine_future(error)]
    Failed(Error),
}

impl PollStateMachine for StateMachine {
    fn poll_start<'a, 'c>(
        _state: &'a mut RentToOwn<'a, Start>,
        context: &'c mut RentToOwn<'c, Context>,
    ) -> Poll<AfterStart, Error> {
        // Abort if the output stream has been closed. Depending on how the
        // network indexer is wired up, this could mean that the system shutting
        // down.
        try_ready!(context.event_sink.poll_ready());

        debug!(context.logger, "Start");

        // Start by pulling the local head from the store. This is the most
        // recent block we managed to index until now.
        transition!(LoadLocalHead {
            local_head: Box::new(future::result(
                context.store.clone().block_ptr(context.subgraph_id.clone())
            ))
        })
    }

    fn poll_load_local_head<'a, 'c>(
        state: &'a mut RentToOwn<'a, LoadLocalHead>,
        context: &'c mut RentToOwn<'c, Context>,
    ) -> Poll<AfterLoadLocalHead, Error> {
        // Abort if the output stream has been closed.
        try_ready!(context.event_sink.poll_ready());

        debug!(context.logger, "Load local head");

        // Wait until we have the local head block; fail if we can't get it from
        // the store because that means the subgraph is broken.
        let local_head = try_ready!(state.local_head.poll());

        // Move on and identify the latest block on chain.
        transition!(PollChainHead {
            local_head,
            chain_head: poll_chain_head(context.logger.clone(), context.adapter.clone()),
        })
    }

    fn poll_poll_chain_head<'a, 'c>(
        state: &'a mut RentToOwn<'a, PollChainHead>,
        context: &'c mut RentToOwn<'c, Context>,
    ) -> Poll<AfterPollChainHead, Error> {
        // Abort if the output stream has been closed.
        try_ready!(context.event_sink.poll_ready());

        match state.chain_head.poll() {
            // Wait until we have the chain head block.
            Ok(Async::NotReady) => Ok(Async::NotReady),

            // We have a (new?) chain head, decide what to do.
            Ok(Async::Ready(chain_head)) => {
                // Validate the chain head.
                if chain_head.number.is_none() || chain_head.hash.is_none() {
                    warn!(
                        context.logger,
                        "Remote head block number or hash missing; trying again";
                        "block" => format!("{:?}/{:?}", chain_head.number, chain_head.hash),
                    );

                    transition!(PollChainHead {
                        local_head: state.local_head,
                        chain_head: poll_chain_head(
                            context.logger.clone(),
                            context.adapter.clone()
                        ),
                    })
                }

                let state = state.take();

                // Pull number out of the local and chain head; we can safely do this here:
                // the local head is a block pointer (or none), which always have a number,
                // the chain head has just been validated.
                let chain_head_number = chain_head.number.unwrap().as_u64();

                trace!(
                    context.logger,
                    "Identify next blocks to index";
                    "chain_head" => format_light_block(&chain_head),
                    "local_head" => state.local_head.map_or(
                        String::from("none"), |ptr| format_block_pointer(&ptr)
                    ),
                );

                // Calculate the number of blocks remaining before we are in sync with the
                // network; fetch no more than 1000 blocks at a time.
                let next_block_number = state.local_head.map_or(0u64, |ptr| ptr.number + 1);
                let remaining_blocks = chain_head_number + 1 - next_block_number;
                let block_range_size = remaining_blocks.min(1000);
                let block_numbers = next_block_number..(next_block_number + block_range_size);

                info!(
                    context.logger,
                    "Queue {} of {} remaining blocks",
                    block_range_size, remaining_blocks;
                    "chain_head" => format_light_block(&chain_head),
                    "local_head" => state.local_head.map_or(
                        String::from("none"), |ptr| format_block_pointer(&ptr)
                    ),
                    "range" => format!("#{}..#{}", block_numbers.start, block_numbers.end-1),
                );

                // Continue processing blocks from this range.
                transition!(ProcessBlocks {
                    local_head: state.local_head,
                    chain_head,
                    next_blocks: fetch_blocks(
                        context.logger.clone(),
                        context.adapter.clone(),
                        block_numbers
                    )
                })
            }

            Err(e) => {
                trace!(
                    context.logger,
                    "Failed to poll chain head; try again";
                    "error" => format!("{}", e),
                );

                let state = state.take();

                transition!(PollChainHead {
                    local_head: state.local_head,
                    chain_head: poll_chain_head(context.logger.clone(), context.adapter.clone()),
                })
            }
        }
    }

    fn poll_process_blocks<'a, 'c>(
        state: &'a mut RentToOwn<'a, ProcessBlocks>,
        context: &'c mut RentToOwn<'c, Context>,
    ) -> Poll<AfterProcessBlocks, Error> {
        // Abort if the output stream has been closed.
        try_ready!(context.event_sink.poll_ready());

        // Try to read the next block.
        match state.next_blocks.poll() {
            // No block ready yet, try again later.
            Ok(Async::NotReady) => Ok(Async::NotReady),

            // The stream is exhausted, update the chain head and fetch the
            // next range of blocks for processing.
            Ok(Async::Ready(None)) => {
                trace!(context.logger, "Check if there are more blocks");

                let state = state.take();

                transition!(PollChainHead {
                    local_head: state.local_head,
                    chain_head: poll_chain_head(context.logger.clone(), context.adapter.clone()),
                })
            }

            // The block could not be fetched but there was also no clear error;
            // try starting over with a fresh chain head.
            Ok(Async::Ready(Some(None))) => {
                trace!(
                    context.logger,
                    "Failed to fetch block, re-evaluate the chain head and try again"
                );

                let state = state.take();

                transition!(PollChainHead {
                    local_head: state.local_head,
                    chain_head: poll_chain_head(context.logger.clone(), context.adapter.clone()),
                })
            }

            // There is a block ready to be processed; check whether it is valid
            // and whether it requires a reorg before adding it
            Ok(Async::Ready(Some(Some(block)))) => {
                let state = state.take();

                transition!(VetBlock {
                    local_head: state.local_head,
                    chain_head: state.chain_head,
                    next_blocks: state.next_blocks,
                    block,
                })
            }

            // Fetching blocks failed; we have no choice but to start over again
            // with a fresh chain head.
            Err(e) => {
                trace!(
                    context.logger,
                    "Failed to fetch block; re-evaluate the chain head and try again";
                    "error" => format!("{}", e),
                );

                let state = state.take();

                transition!(PollChainHead {
                    local_head: state.local_head,
                    chain_head: poll_chain_head(context.logger.clone(), context.adapter.clone()),
                })
            }
        }
    }

    fn poll_vet_block<'a, 'c>(
        state: &'a mut RentToOwn<'a, VetBlock>,
        context: &'c mut RentToOwn<'c, Context>,
    ) -> Poll<AfterVetBlock, Error> {
        // Abort if the output stream has been closed.
        try_ready!(context.event_sink.poll_ready());

        let state = state.take();
        let block = state.block;

        // Validate the block.
        if block.inner().number.is_none() || block.inner().hash.is_none() {
            warn!(
                context.logger,
                "Block number or hash missing; trying again";
                "block" => format_block(&block),
            );

            // The block is invalid, throw away the entire stream and
            // start with re-checking the chain head block again.
            transition!(PollChainHead {
                local_head: state.local_head,
                chain_head: poll_chain_head(context.logger.clone(), context.adapter.clone()),
            })
        }

        // If we encounter a block that has a smaller number than our
        // local head block, then we throw away the block stream and
        // try to start over with a fresh chain head block.
        let block_number = block.inner().number.unwrap().as_u64();
        let local_head_number = state.local_head.map_or(0u64, |ptr| ptr.number);
        if block_number < local_head_number {
            warn!(
                context.logger,
                "Received an older block than the local head; \
                 re-evaluate chain head and try again";
                "local_head_number" => format!("{}", local_head_number),
                "block" => format_block(&block),
            );

            transition!(PollChainHead {
                local_head: state.local_head,
                chain_head: poll_chain_head(context.logger.clone(), context.adapter.clone()),
            })
        }

        // Check whether we have a reorg (parent of the block != our local head).
        if block.inner().parent_ptr() != state.local_head {
            info!(
                context.logger,
                "Block requires a reorg";
                "block" => format_block(&block),
            );

            // We are dealing with a reorg; fetch all blocks from the new
            // block back to the most recent block that it is also an ancestor
            // of the local head block. That block is the "fork base", i.e.,
            // the block after which the chain was forked.
            transition!(FetchForkedBlocks {
                local_head: state.local_head,
                chain_head: state.chain_head,
                next_blocks: state.next_blocks,
                forked_blocks: fetch_forked_blocks(
                    context.logger.clone(),
                    context.subgraph_id.clone(),
                    context.adapter.clone(),
                    context.store.clone(),
                    block
                ),
            })
        } else {
            let event_sink = context.event_sink.clone();

            // The block is a regular successor to the current local head block.
            // Add the block and move on.
            transition!(AddBlock {
                // Remember the old local head in case we need to roll back.
                old_local_head: state.local_head,

                // Carry over the current chain head and the incoming blocks stream.
                chain_head: state.chain_head,
                next_blocks: state.next_blocks,

                // Index the block.
                new_local_head: Box::new(
                    // Write block to the store.
                    write_block(context.block_writer.clone(), block)
                        // Send an `AddBlock` event for it.
                        .and_then(move |block_ptr| {
                            send_event(event_sink, NetworkIndexerEvent::AddBlock(block_ptr.clone()))
                                .and_then(move |_| {
                                    // Return the new block so we can update the local head.
                                    future::ok(block_ptr)
                                })
                        })
                )
            })
        }
    }

    fn poll_fetch_forked_blocks<'a, 'c>(
        state: &'a mut RentToOwn<'a, FetchForkedBlocks>,
        context: &'c mut RentToOwn<'c, Context>,
    ) -> Poll<AfterFetchForkedBlocks, Error> {
        // Abort if the output stream has been closed.
        try_ready!(context.event_sink.poll_ready());

        match state.forked_blocks.poll() {
            // Don't have the forked blocks yet, try again later
            Ok(Async::NotReady) => Ok(Async::NotReady),

            // Have the forked blocks, now revert to the fork base and
            // then add the forked blocks to move forward again.
            Ok(Async::Ready(mut forked_blocks)) => {
                let state = state.take();

                let fork_base = forked_blocks
                    .pop()
                    .expect("can't have a reorg without a fork base");

                let fork_base_ptr = fork_base.inner().into();
                let local_head_ptr = state
                    .local_head
                    .expect("cannot have a reorg if there is no local head block yet")
                    .into();

                let subgraph_id_for_revert = context.subgraph_id.clone();
                let logger_for_revert = context.logger.clone();
                let store_for_revert = context.store.clone();
                let event_sink_for_revert = context.event_sink.clone();

                transition!(RevertToForkBase {
                    local_head: state.local_head,
                    chain_head: state.chain_head,

                    // Make the blocks from the forked branch the next ones to process
                    // before any other incoming blocks
                    next_blocks: Box::new(
                        stream::iter_ok(forked_blocks.into_iter().map(|block| Some(block)).rev())
                            .chain(state.next_blocks)
                    ),

                    // Identify the sequence of block pointers we need to revert,
                    // going back from `local head` to `fork_base`; then revert
                    // all of those by emitting revert events
                    new_local_head: Box::new(
                        collect_blocks_to_revert(
                            context.logger.clone(),
                            context.subgraph_id.clone(),
                            context.store.clone(),
                            local_head_ptr,
                            fork_base_ptr,
                        )
                        .and_then(move |block_ptrs| {
                            revert_blocks(
                                subgraph_id_for_revert,
                                logger_for_revert,
                                store_for_revert,
                                event_sink_for_revert,
                                block_ptrs,
                            )
                        })
                    )
                })
            }

            // Fetching the forked blocks failed, reset to identifying
            // the chain head again
            Err(e) => {
                trace!(
                    context.logger,
                    "Fetching forked blocks failed; \
                     re-evaluate chain head and try again";
                    "error" => format!("{}", e)
                );

                let state = state.take();

                transition!(PollChainHead {
                    local_head: state.local_head,
                    chain_head: poll_chain_head(context.logger.clone(), context.adapter.clone()),
                })
            }
        }
    }

    fn poll_revert_to_fork_base<'a, 'c>(
        state: &'a mut RentToOwn<'a, RevertToForkBase>,
        context: &'c mut RentToOwn<'c, Context>,
    ) -> Poll<AfterRevertToForkBase, Error> {
        // Abort if the output stream has been closed.
        try_ready!(context.event_sink.poll_ready());

        match state.new_local_head.poll() {
            // Reverting has not finished yet, try again later.
            Ok(Async::NotReady) => Ok(Async::NotReady),

            // The revert finished and the fork base should become our new
            // local head. Continue processing the blocks that we pulled in
            // for the reorg.
            Ok(Async::Ready(block_ptr)) => {
                let state = state.take();

                transition!(ProcessBlocks {
                    // Set the local head to the block we have reverted to
                    local_head: Some(block_ptr),
                    chain_head: state.chain_head,
                    next_blocks: state.next_blocks,
                })
            }
            // There was an error reverting; re-evaluate the chain head
            // and try again.
            Err(e) => {
                warn!(
                    context.logger,
                    "Failed to handle reorg, re-evaluate the chain head and try again";
                    "error" => format!("{}", e),
                );

                let state = state.take();

                transition!(PollChainHead {
                    local_head: state.local_head,
                    chain_head: poll_chain_head(context.logger.clone(), context.adapter.clone()),
                })
            }
        }
    }

    fn poll_add_block<'a, 'c>(
        state: &'a mut RentToOwn<'a, AddBlock>,
        context: &'c mut RentToOwn<'c, Context>,
    ) -> Poll<AfterAddBlock, Error> {
        // Abort if the output stream has been closed.
        try_ready!(context.event_sink.poll_ready());

        match state.new_local_head.poll() {
            // Adding the block is not complete yet, try again later.
            Ok(Async::NotReady) => return Ok(Async::NotReady),

            // We have the new local block, update it and continue processing blocks.
            Ok(Async::Ready(block_ptr)) => {
                let state = state.take();

                transition!(ProcessBlocks {
                    local_head: Some(block_ptr),
                    chain_head: state.chain_head,
                    next_blocks: state.next_blocks,
                })
            }

            // Something went wrong, back to re-evaluating the chain head it is!
            Err(e) => {
                trace!(
                    context.logger,
                    "Failed to add block, re-evaluate the chain head and try again";
                    "error" => format!("{}", e),
                );

                let state = state.take();

                transition!(PollChainHead {
                    local_head: state.old_local_head,
                    chain_head: poll_chain_head(context.logger.clone(), context.adapter.clone()),
                })
            }
        }
    }
}

pub struct NetworkIndexer {
    output: Option<Receiver<NetworkIndexerEvent>>,
}

impl NetworkIndexer {
    pub fn new<S>(
        subgraph_id: SubgraphDeploymentId,
        logger: &Logger,
        adapter: Arc<dyn EthereumAdapter>,
        store: Arc<S>,
        metrics_registry: Arc<dyn MetricsRegistry>,
    ) -> Self
    where
        S: Store + ChainStore,
    {
        let logger = logger.new(o!("component" => "NetworkIndexer"));
        let logger_for_err = logger.clone();

        let stopwatch = StopwatchMetrics::new(
            logger.clone(),
            subgraph_id.clone(),
            metrics_registry.clone(),
        );

        let block_writer = Arc::new(BlockWriter::new(
            subgraph_id.clone(),
            &logger,
            store.clone(),
            stopwatch,
            metrics_registry.clone(),
        ));

        // Create a channel for emitting events
        let (event_sink, output) = channel(100);

        // Create state machine that emits block and revert events for the network
        let state_machine = StateMachine::start(Context {
            subgraph_id,
            logger,
            adapter,
            store,
            event_sink,
            block_writer,
        });

        // Launch state machine
        tokio::spawn(state_machine.map_err(move |e| {
            error!(logger_for_err, "Network indexer failed: {}", e);
        }));

        Self {
            output: Some(output),
        }
    }
}

impl EventProducer<NetworkIndexerEvent> for NetworkIndexer {
    fn take_event_stream(
        &mut self,
    ) -> Option<Box<dyn Stream<Item = NetworkIndexerEvent, Error = ()> + Send>> {
        self.output
            .take()
            .map(|s| Box::new(s) as Box<dyn Stream<Item = NetworkIndexerEvent, Error = ()> + Send>)
    }
}
