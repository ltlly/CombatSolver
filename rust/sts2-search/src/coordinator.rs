use crate::arena::{
    ChoiceArena, CompactAction, CompactActionKind, LocalStateSlab, MainStateArena, NodeArena,
    NodeId, PackedHandle, SearchNode, NODE_FLAG_HAS_CHOICE_LOOKAHEAD, NODE_FLAG_HAS_EXTRA_END_TURN,
    NODE_FLAG_HAS_FULL_EVAL, NODE_FLAG_HAS_ROLLOUT_SCORE, NODE_FLAG_IS_TERMINAL,
};
use crate::boss_tempo::{
    retain_boss_tempo_prefixes, BossTempoPendingItem, BossTempoPrefix, BossTempoSearchOptions,
    BossTempoTranspositions,
};
use crate::early_turn::{
    interleave_potion_frontiers, select_early_turn_continuation_incumbent,
    select_early_turn_frontier, EARLY_TURN_EXPLORATION_INITIAL_RANK_LIMIT,
    EARLY_TURN_EXPLORATION_MAX_RANK_LIMIT, EARLY_TURN_EXPLORATION_NODE_BUDGET,
};
use crate::eval::{
    allows_brightest_flame, compute_beam_rank_score_with_centi, evaluate_fast,
    evaluate_fast_and_full_fused, evaluate_full, evaluate_full_details_cached, select_final_plan,
    ActEndingBossPolicy, EvalMode, FastEvalSummary, FullEvalDetails, IntentHitsCache,
    PaidPotionImproveInput, PotionUsePolicy, PotionValuationRegistry, PowerCardValuationRegistry,
    PowerRoutePriority, RareNodeEval, RelicCounterEvaluation, RelicCounterId, RootEvalBaseline,
    SolverPotionPolicy, SolverTheftPolicy, StoredFastEval, StoredNodeEval,
    FAST_FLAG_FORFEITS_GOLD,
};
use crate::hash::StateHash;
use crate::novelty::{
    capture_novelty_facts, BfwsBoundedOpen, BfwsEscapeBudget, BfwsFact, BfwsPackedNovelty,
    NoveltyOpenNode, NoveltyPortfolioBudget, NoveltySearchOptions,
};
use crate::packer::{empty_scratch_state, hash_canonical_key, pack_state, unpack_state};
use crate::por::{
    card_equiv_key, classify_card_for_por, dedup_and_filter_potions_with_root,
    dedup_playable_hand_cards, enumerate_canonical_choices_with_scratch,
    has_any_playable_hand_card, has_any_usable_potion_with_root, por_should_skip,
    ChoiceEnumScratch, CombatOrderSensitivity, HandCardEquivKey, PorActionClass,
};
use crate::portfolio::{
    BeamWidthPortfolio, BeamWidthPortfolioBaseline, BeamWidthPortfolioGate,
    BeamWidthPortfolioRun, PortfolioSchedulingMode, PowerCommitmentPortfolioGate,
    SolverSearchProfile,
};
use crate::pruning::{
    compute_effective_net_hp_loss_centi, compute_tactical_signature, BeamRetentionPolicy,
    ClosureCertificate, ExtendedPruneEntry, FrozenIncumbentBound, PrimaryIncumbentBucket,
    PrimaryIncumbentTable, PrimarySearchIncumbent, PruneKey, PruneScratch, PruneSource,
};
use crate::rollout::{
    encode_choice_probe_rank_score, evaluate_choice_probe_in_place,
    evaluate_choice_probe_in_place_with_restore, evaluate_choice_probe_packed,
    evaluate_turn_boundary_rollouts_into, ChoiceProbeCache, ChoiceProbeResult,
    ChoiceProbeWorkspace, ProbeCacheAccess, RolloutResult, TurnRolloutCache,
};
use crate::tt::{ShardedParetoTT, TranspositionLabel, TT_SHARD_COUNT};
use crate::{InCombatSolveResponse, PlannedAction, SearchPolicy, SearchProgress, SearchStatistics};
use rayon::prelude::*;
use smallvec::SmallVec;
use std::borrow::Cow;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use sts2_kernel::content_dispatch::{rollout_meta_flags, rollout_op_flags};
use sts2_kernel::state::{
    ActiveHookCache, BattleState, CardHandle, CreatureHandle, FieldMap, GrowthCounters,
    RelicDirtyMask, RootStaticContext, SemanticHistory, HOOK_DIRTY_ALL,
};
use sts2_kernel::{CombatEngine, ContentDispatcher, KernelError, KernelErrorKind, StepOutcome};
use sts2_types::{
    CardStaticFlags, CardTags, CardType, PileType, PlayerTurnPhase, PotionId, PowerId, RoomType,
    TargetType,
};

struct SearchAbortSlot {
    tripped: AtomicBool,
    first_error: parking_lot::Mutex<Option<KernelError>>,
}

impl SearchAbortSlot {
    fn new() -> Self {
        Self {
            tripped: AtomicBool::new(false),
            first_error: parking_lot::Mutex::new(None),
        }
    }

    #[inline(always)]
    fn is_tripped(&self) -> bool {
        self.tripped.load(Ordering::Acquire)
    }

    #[cold]
    #[inline(never)]
    fn record_error(&self, err: KernelError) {
        let mut slot = self.first_error.lock();
        if slot.is_none() {
            *slot = Some(err);
        }
        self.tripped.store(true, Ordering::Release);
    }

    #[inline(always)]
    fn clone_error(&self) -> Option<KernelError> {
        if !self.is_tripped() {
            return None;
        }
        self.first_error.lock().clone()
    }
}

#[inline(always)]
fn should_abort_search_on_kernel_error(err: &KernelError) -> bool {
    matches!(
        err.kind(),
        KernelErrorKind::Unsupported { .. }
            | KernelErrorKind::InvalidState(_)
            | KernelErrorKind::Dec(_)
            | KernelErrorKind::Rng(_)
            | KernelErrorKind::SnapshotImport { .. }
    )
}

const CHOICE_RESOLVED_CAPACITY: usize = 160;
const TURN_SETUP_CHOICE_RESOLVED_CAPACITY: usize = 384;
const CHOICE_TT_SIZE: usize = 1024;
const CHOICE_TT_MASK: usize = CHOICE_TT_SIZE - 1;
const MAX_BEAM_WIDTH: usize = 640;

#[derive(Clone, Copy, Debug)]
enum ActionSpec {
    TurnSetup,
    PlayCard {
        card_handle: CardHandle,
        target_handle: Option<CreatureHandle>,
        por_class: PorActionClass,
    },
    UsePotion {
        potion_slot: usize,
        potion_id: PotionId,
        target_handle: Option<CreatureHandle>,
        is_generated: bool,
        is_forced: bool,
        optional_paid_cost_delta: i32,
        optional_paid_count_delta: u16,
        root_potion_slot_bit: u16,
        forced_bit: Option<u32>,
    },
    EndTurn,
}

#[derive(Clone)]
struct ActionTask {
    parent_rank: u32,
    parent_id: NodeId,
    parent_turn: i32,
    parent_action_count: u16,
    parent_potion_count: u16,
    parent_explicit_potion_count: u16,
    parent_optional_paid_potion_count: u16,
    parent_automatic_potion_count: u16,
    parent_filled_potions: u8,
    parent_used_potion_slot_mask: u16,
    parent_potion_cost: i32,
    parent_optional_paid_potion_cost: i32,
    parent_ambergris_count: u16,
    parent_forced_used_mask: u64,
    parent_forced_ambergris_count: u16,
    parent_has_choice_lookahead: bool,
    parent_choice_lookahead_score: Option<f64>,
    action_ord: u32,
    spec: ActionSpec,
}

#[derive(Clone, Copy, Debug)]
struct PassSearchOptions {
    allow_paid_optional_potions: bool,
    prefer_potion_buckets: bool,
    eval_mode: EvalMode,
    early_turn_scout_depth: i32,
    boss_tempo_scout_turns: i32,
    rollout_horizon_turns: usize,
    allow_death_save_rescue: bool,
    relax_zero_potion_reserve: bool,
    min_zero_potion_reserve_percent: i32,
}

impl Default for PassSearchOptions {
    fn default() -> Self {
        Self {
            allow_paid_optional_potions: true,
            prefer_potion_buckets: false,
            eval_mode: EvalMode::Balanced,
            early_turn_scout_depth: 0,
            boss_tempo_scout_turns: 0,
            rollout_horizon_turns: 0,
            allow_death_save_rescue: false,
            relax_zero_potion_reserve: false,
            min_zero_potion_reserve_percent: 0,
        }
    }
}

struct WorkerCandidate {
    sort_key: (u32, u32, u32), // (parent_rank, action_ord, choice_ord)
    parent_id: NodeId,
    action: CompactAction,
    por_key: u64,
    has_extra_end_turn: bool,
    choices_offset: u32,
    choices_len: u8,
    state_slab_idx: u32,
    state_off: u32,
    state_len: u32,
    state_hash: StateHash,
    turn: i32,
    turn_completed: bool,
    action_count: u16,
    potion_count: u16,
    explicit_potion_count: u16,
    optional_paid_potion_count: u16,
    automatic_potion_count: u16,
    used_potion_slot_mask: u16,
    potion_strategic_cost: i32,
    optional_paid_potion_cost: i32,
    ambergris_count: u16,
    forced_used_mask: u64,
    forced_ambergris_count: u16,
    is_terminal: bool,
    base_score: f64,
    fast_eval: StoredFastEval,
    full_eval: Option<StoredNodeEval>,
    rare_eval: Option<RareNodeEval>,
    label: TranspositionLabel,
}

#[derive(Clone, Copy)]
struct ResolvedChoiceCandidate {
    choices_offset: u32,
    choices_len: u8,
    depth0_sig: u64,
    action: CompactAction,
    por_key: u64,
    has_extra_end_turn: bool,
    state_slab_idx: u32,
    state_off: u32,
    state_len: u32,
    state_hash: StateHash,
    turn: i32,
    turn_completed: bool,
    action_count: u16,
    potion_count: u16,
    explicit_potion_count: u16,
    optional_paid_potion_count: u16,
    automatic_potion_count: u16,
    used_potion_slot_mask: u16,
    potion_strategic_cost: i32,
    optional_paid_potion_cost: i32,
    ambergris_count: u16,
    forced_used_mask: u64,
    forced_ambergris_count: u16,
    is_terminal: bool,
    base_score: f64,
    fast_eval: StoredFastEval,
    full_eval: Option<StoredNodeEval>,
    rare_eval: Option<RareNodeEval>,
    label: TranspositionLabel,
    terminal_probe_score: f64,
    lookahead_score: f64,
    lookahead_hp_lost: i32,
    lookahead_board_investment: i32,
    lookahead_setup_score: i32,
    has_lookahead: bool,
    lookahead_victory: bool,
}

struct DeferredChoiceBatch {
    task_idx: usize,
    parent_rank: u32,
    action_ord: u32,
    parent_id: NodeId,
    max_branches: usize,
    probe_max_turns_ahead: usize,
    probed_pool: Vec<usize>,
    selected_indices: SmallVec<[usize; 64]>,
    resolved: Vec<ResolvedChoiceCandidate>,
    slab: LocalStateSlab,
    choices: ChoiceArena,
    probe_results: Vec<Option<ChoiceProbeResult>>,
}

#[derive(Clone, Copy)]
struct ProbeJob {
    batch_idx: u32,
    cand_idx: u32,
    state_slab_idx: u32,
    state_off: u32,
    state_len: u32,
    state_hash: StateHash,
    opt_cost: i32,
    opt_count: usize,
    action_count: u16,
}

struct WorkerScratch {
    state: BattleState,
    pre_end_turn_state: BattleState,
    choice_paused_states: Vec<BattleState>,
    choice_probe_ws: ChoiceProbeWorkspace,
    eval_intent_cache: IntentHitsCache,
    choice_resolved: Vec<ResolvedChoiceCandidate>,
    choice_enum_bufs: Vec<Vec<SmallVec<[usize; 8]>>>,
    choice_enum_scratch: ChoiceEnumScratch,
    choice_slab: LocalStateSlab,
    choice_staging_choices: ChoiceArena,
    staged_choices: ChoiceArena,
    choice_tt_hashes: [StateHash; CHOICE_TT_SIZE],
    choice_tt_epochs: [u32; CHOICE_TT_SIZE],
    choice_tt_epoch: u32,
    marginal_scores: Vec<f64>,
    marginal_order: Vec<usize>,
    probed_sels: Vec<(SmallVec<[usize; 8]>, f64, u64, u64)>,
    deferred_batches: Vec<DeferredChoiceBatch>,
    deferred_probe_out: Vec<(usize, Option<ChoiceProbeResult>)>,
    pack_buf: Vec<u8>,
    action_buf: Vec<ActionTask>,
    slab: LocalStateSlab,
    candidates: Vec<WorkerCandidate>,
    cand_shards: Vec<u16>,
    cand_fast_meta: Vec<WorkerCandFastMeta>,
    transitions: u64,
    choice_replay_attempts: u64,
    incumbent_pruned: u64,
    tt_probe_pruned: u64,
    por_pruned: u64,
    branch_truncations: u64,
    wave_dirty: bool,
    fatal_error: Option<KernelError>,
}

impl WorkerScratch {
    fn new(static_ctx: Arc<RootStaticContext>) -> Self {
        let choice_paused_states = (0..4)
            .map(|_| empty_scratch_state(static_ctx.clone()))
            .collect();
        Self {
            pre_end_turn_state: empty_scratch_state(static_ctx.clone()),
            choice_paused_states,
            choice_probe_ws: ChoiceProbeWorkspace::new(static_ctx.clone()),
            eval_intent_cache: IntentHitsCache::new(),
            choice_resolved: Vec::with_capacity(TURN_SETUP_CHOICE_RESOLVED_CAPACITY),
            choice_enum_bufs: vec![
                Vec::with_capacity(64),
                Vec::with_capacity(32),
                Vec::with_capacity(16),
                Vec::with_capacity(16),
            ],
            choice_enum_scratch: ChoiceEnumScratch::default(),
            choice_slab: LocalStateSlab::default(),
            choice_staging_choices: ChoiceArena::default(),
            staged_choices: ChoiceArena::default(),
            choice_tt_hashes: [StateHash::default(); CHOICE_TT_SIZE],
            choice_tt_epochs: [0u32; CHOICE_TT_SIZE],
            choice_tt_epoch: 0,
            marginal_scores: Vec::with_capacity(64),
            marginal_order: Vec::with_capacity(64),
            probed_sels: Vec::with_capacity(64),
            deferred_batches: Vec::new(),
            deferred_probe_out: Vec::new(),
            state: empty_scratch_state(static_ctx),
            pack_buf: Vec::with_capacity(4096),
            action_buf: Vec::with_capacity(16),
            slab: LocalStateSlab::default(),
            candidates: Vec::with_capacity(64),
            cand_shards: Vec::with_capacity(64),
            cand_fast_meta: Vec::with_capacity(64),
            transitions: 0,
            choice_replay_attempts: 0,
            incumbent_pruned: 0,
            tt_probe_pruned: 0,
            por_pruned: 0,
            branch_truncations: 0,
            wave_dirty: false,
            fatal_error: None,
        }
    }

    fn reset_for_wave(&mut self) {
        self.slab.clear();
        self.staged_choices.clear();
        self.choice_slab.clear();
        self.choice_staging_choices.clear();
        self.candidates.clear();
        self.cand_shards.clear();
        self.cand_fast_meta.clear();
        self.choice_resolved.clear();
        self.deferred_batches.clear();
        self.deferred_probe_out.clear();
        self.transitions = 0;
        self.choice_replay_attempts = 0;
        self.incumbent_pruned = 0;
        self.tt_probe_pruned = 0;
        self.por_pruned = 0;
        self.branch_truncations = 0;
        self.wave_dirty = false;
        self.fatal_error = None;
    }

    #[cold]
    #[inline(never)]
    fn record_fatal_error(&mut self, err: KernelError) {
        if self.fatal_error.is_none() {
            self.fatal_error = Some(err);
        }
    }

    #[inline]
    fn ensure_choice_depth(&mut self, depth: usize, static_ctx: &Arc<RootStaticContext>) {
        while self.choice_paused_states.len() <= depth {
            self.choice_paused_states
                .push(empty_scratch_state(static_ctx.clone()));
        }
        while self.choice_enum_bufs.len() <= depth {
            self.choice_enum_bufs.push(Vec::with_capacity(16));
        }
    }

    #[inline]
    fn begin_choice_tree(&mut self) {
        self.choice_slab.clear();
        self.choice_staging_choices.clear();
        self.choice_resolved.clear();
        self.choice_tt_epoch = self.choice_tt_epoch.wrapping_add(1);
        if self.choice_tt_epoch == 0 {
            self.choice_tt_epochs.fill(0);
            self.choice_tt_epoch = 1;
        }
    }

    #[inline]
    fn try_insert_local_choice_tt(&mut self, hash: StateHash) -> bool {
        let mut slot = (hash.0 as usize) & CHOICE_TT_MASK;
        for _ in 0..CHOICE_TT_SIZE {
            if self.choice_tt_epochs[slot] != self.choice_tt_epoch {
                self.choice_tt_epochs[slot] = self.choice_tt_epoch;
                self.choice_tt_hashes[slot] = hash;
                return true;
            }
            if self.choice_tt_hashes[slot] == hash {
                return false;
            }
            slot = (slot + 1) & CHOICE_TT_MASK;
        }
        true
    }
}

#[inline(always)]
fn pack_parent_slice(w_idx: usize, start: usize, len: usize) -> u64 {
    debug_assert!(w_idx < 256 && start < (1 << 28) && len < (1 << 28));
    ((w_idx as u64) << 56)
        | (((start as u64) & 0x0FFF_FFFF) << 28)
        | ((len as u64) & 0x0FFF_FFFF)
}

#[inline(always)]
fn unpack_parent_slice(raw: u64) -> (usize, usize, usize) {
    let w_idx = (raw >> 56) as usize;
    let start = ((raw >> 28) & 0x0FFF_FFFF) as usize;
    let len = (raw & 0x0FFF_FFFF) as usize;
    (w_idx, start, len)
}

#[derive(Clone, Copy)]
#[repr(C)]
struct WorkerCandFastMeta {
    flags: u8, // bit 0 = is_terminal, bit 1 = turn_completed
    potion_count: u8,
    explicit_potion_count: u8,
    _pad: [u8; 5],
    beam_score: f64,
}

const _: () = assert!(std::mem::size_of::<WorkerCandFastMeta>() == 16);

#[derive(Clone, Copy)]
#[repr(C)]
struct AcceptedMidTurn {
    w_idx: u16,
    potion_count: u8,
    explicit_potion_count: u8,
    c_idx: u32,
    beam_score: f64,
}

const _: () = assert!(std::mem::size_of::<AcceptedMidTurn>() == 16);

#[derive(Clone, Copy)]
enum ParentStateSource<'a> {
    Main(&'a MainStateArena),
    WorkerSlabs(&'a [LocalStateSlab]),
}

impl<'a> ParentStateSource<'a> {
    #[inline(always)]
    fn get(self, h: PackedHandle) -> &'a [u8] {
        match self {
            Self::Main(arena) => arena.get(h),
            Self::WorkerSlabs(slabs) => {
                let w_idx = (h.slab >> 16) as usize;
                let s_idx = h.slab & 0xFFFF;
                slabs[w_idx].get(s_idx, h.offset, h.len)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
struct StandPatParentKey {
    forced_used_mask: u64,
    used_potion_slot_mask: u16,
    potion_strategic_cost: i16,
    optional_paid_potion_cost: i16,
    action_count: u16,
    potion_count: u8,
    explicit_potion_count: u8,
    optional_paid_potion_count: u8,
    ambergris_count: u8,
    forced_ambergris_count: u8,
}

impl StandPatParentKey {
    #[inline(always)]
    fn from_node(n: &SearchNode) -> Self {
        Self {
            forced_used_mask: n.forced_used_mask,
            used_potion_slot_mask: n.used_potion_slot_mask,
            potion_strategic_cost: n.potion_strategic_cost,
            optional_paid_potion_cost: n.optional_paid_potion_cost,
            action_count: n.action_count,
            potion_count: n.potion_count,
            explicit_potion_count: n.explicit_potion_count,
            optional_paid_potion_count: n.optional_paid_potion_count,
            ambergris_count: n.ambergris_count,
            forced_ambergris_count: n.forced_ambergris_count,
        }
    }
}

struct PassResources {
    scratches: Vec<WorkerScratch>,
    parent_states: Vec<BattleState>,
    enumerated_tasks: Vec<Vec<ActionTask>>,
    rollout_workspaces: Vec<crate::rollout::RolloutWorkspace>,
    node_arena: NodeArena,
    prune_scratch: PruneScratch,
    flat_tasks: Vec<(usize, ActionTask)>,
    deferred_batches: Vec<DeferredChoiceBatch>,
    probe_jobs: Vec<ProbeJob>,
    step_arena: MainStateArena,
    prev_worker_slabs: Vec<LocalStateSlab>,
    turn_staging_arena: MainStateArena,
    tt: ShardedParetoTT,
    choice_probe_cache: ChoiceProbeCache,
    turn_rollout_cache: TurnRolloutCache,
    stand_pat_seen: hashbrown::HashSet<(StateHash, StandPatParentKey), ahash::RandomState>,
    sort_list: Vec<(usize, usize, (u32, u32, u32))>,
    parent_slices: Vec<AtomicU64>,
    active_parent_slices: Vec<(u32, u16, u32, u32)>,
    tt_group_accepted: Box<[Vec<u32>; 16]>,
    tt_accepted_epoch: Vec<u8>,
    tt_cur_epoch: u8,
    tt_shard_buckets: Vec<Vec<(usize, StateHash, TranspositionLabel)>>,
    tt_accepted_flags: Vec<AtomicU8>,
    tt_flat_refs: Vec<(usize, usize)>,
    accepted_mid_turn: Vec<AcceptedMidTurn>,
    mid_turn_ords: Vec<NodeId>,
    surviving_ords: Vec<NodeId>,
    surviving_commit_order: Vec<usize>,
    rollout_results: Vec<Option<RolloutResult>>,
}

struct WorkerCandidateSource<'a> {
    scratches: &'a [WorkerScratch],
    accepted: &'a [AcceptedMidTurn],
}

impl PruneSource for WorkerCandidateSource<'_> {
    #[inline(always)]
    fn build_prune_entry(
        &self,
        id: NodeId,
        root: &RootEvalBaseline,
        use_rollout_scores: bool,
    ) -> (PruneKey, i64, i64, i64, i64, f64, StateHash) {
        let acc = &self.accepted[id.0 as usize];
        let cand = &self.scratches[acc.w_idx as usize].candidates[acc.c_idx as usize];
        let f = &cand.fast_eval;
        let potion_cost = cand
            .potion_strategic_cost
            .clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        let explicit_potion_count = acc.explicit_potion_count;
        let (
            has_full_eval,
            beam_rank_score,
            has_rollout_score,
            has_choice_lookahead,
            stored_aux_score,
            effective_defense,
            board_investment,
            setup_score,
            optionality_cards,
        ) = if let Some(ref full) = cand.full_eval {
            let has_ro = full.rollout_score.is_some();
            let aux = if let Some(rs) = full.rollout_score {
                rs
            } else {
                full.choice_lookahead_score.unwrap_or(0.0)
            };
            (
                true,
                full.beam_rank_score,
                has_ro,
                full.has_choice_lookahead,
                aux,
                full.effective_defense.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                full.board_investment.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                full.setup_score.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                full.optionality_cards.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            )
        } else {
            (false, cand.base_score, false, false, 0.0, 0, 0, 0, 0)
        };
        let (k, s, t, su, o, a) = PruneKey::from_raw_fields(
            id,
            root,
            use_rollout_scores,
            f.enemy_hp,
            f.player_hp,
            f.player_max_hp,
            f.player_block,
            f.projected_player_hp,
            f.cumulative_hp_lost,
            f.recovered_hp,
            f.strategic_hp_credit,
            f.alive_enemy_count,
            f.reviving_enemy_count,
            f.death_save_use_count,
            f.strategy_goal_count,
            f.feed_count,
            f.energy,
            f.stars,
            cand.forced_used_mask,
            cand.used_potion_slot_mask,
            potion_cost,
            explicit_potion_count,
            has_full_eval,
            beam_rank_score,
            has_rollout_score,
            has_choice_lookahead,
            stored_aux_score,
            effective_defense,
            board_investment,
            setup_score,
            optionality_cards,
        );
        (k, s, t, su, o, a, cand.state_hash)
    }

    #[inline(always)]
    fn potion_count(&self, id: NodeId) -> u8 {
        self.accepted[id.0 as usize].potion_count
    }

    #[inline(always)]
    fn explicit_potion_count(&self, id: NodeId) -> u8 {
        self.accepted[id.0 as usize].explicit_potion_count
    }

    #[inline(always)]
    fn optional_paid_potion_count(&self, id: NodeId) -> u8 {
        let acc = &self.accepted[id.0 as usize];
        self.scratches[acc.w_idx as usize].candidates[acc.c_idx as usize]
            .optional_paid_potion_count
            .min(u8::MAX as u16) as u8
    }

    #[inline(always)]
    fn forced_used_mask(&self, id: NodeId) -> u64 {
        let acc = &self.accepted[id.0 as usize];
        self.scratches[acc.w_idx as usize].candidates[acc.c_idx as usize].forced_used_mask
    }

    #[inline(always)]
    fn node_turn(&self, id: NodeId) -> i32 {
        let acc = &self.accepted[id.0 as usize];
        self.scratches[acc.w_idx as usize].candidates[acc.c_idx as usize].turn
    }

    #[inline(always)]
    fn beam_score(&self, id: NodeId, use_rollout_scores: bool) -> f64 {
        let acc = &self.accepted[id.0 as usize];
        if !use_rollout_scores {
            return acc.beam_score;
        }
        let cand = &self.scratches[acc.w_idx as usize].candidates[acc.c_idx as usize];
        if let Some(ref full) = cand.full_eval {
            if full.rollout_score.is_some() {
                full.effective_turn_boundary_score()
            } else {
                full.beam_rank_score
            }
        } else {
            cand.base_score
        }
    }

    #[inline(always)]
    fn tie_break_fields(&self, id: NodeId) -> (i16, i32, bool, f64, StateHash) {
        let acc = &self.accepted[id.0 as usize];
        let cand = &self.scratches[acc.w_idx as usize].candidates[acc.c_idx as usize];
        let (hcla, cla) = if let Some(ref full) = cand.full_eval {
            let v = if full.has_choice_lookahead {
                full.rollout_score
                    .or(full.choice_lookahead_score)
                    .unwrap_or(0.0)
            } else {
                f64::NEG_INFINITY
            };
            (full.has_choice_lookahead, v)
        } else {
            (false, f64::NEG_INFINITY)
        };
        (
            cand.fast_eval.cumulative_hp_lost,
            cand.fast_eval.enemy_hp,
            hcla,
            cla,
            cand.state_hash,
        )
    }

    #[inline]
    fn build_extended_prune_entry(
        &self,
        id: NodeId,
        k: &PruneKey,
        root: &RootEvalBaseline,
        _use_rollout_scores: bool,
    ) -> ExtendedPruneEntry {
        let acc = &self.accepted[id.0 as usize];
        let cand = &self.scratches[acc.w_idx as usize].candidates[acc.c_idx as usize];
        let f = &cand.fast_eval;
        let rare = cand.rare_eval.unwrap_or_default();
        let has_meat = root
            .active_relic_targets
            .iter()
            .any(|t| t.id == RelicCounterId::MeatOnTheBone);
        let won = f.won();
        let eff_net_loss_centi = compute_effective_net_hp_loss_centi(
            root,
            has_meat,
            f.player_hp as i32,
            f.player_max_hp as i32,
            f.projected_player_hp as i32,
            f.cumulative_hp_lost as i32,
            f.recovered_hp as i32,
            f.death_save_hp_restored as i32,
            f.death_save_use_count,
            f.feed_count,
            won,
        );
        let eff_def = cand
            .full_eval
            .as_ref()
            .map_or(f.player_block as i32, |full| full.effective_defense);
        let max_dark_evoke = cand
            .full_eval
            .as_ref()
            .map_or(0, |full| full.max_dark_evoke);
        let total_poison_and_doom = cand
            .full_eval
            .as_ref()
            .map_or(0, |full| full.total_poison_and_doom);
        let has_pure_growth = !root.ignore_long_term_rewards && root.effective_has_growth_targets;
        let has_relic = !root.active_relic_targets.is_empty();
        let preserve_theft = root.theft_policy == SolverTheftPolicy::PreserveResources;
        let growth_hp_credit = (f.strategy_goal_hp_credit as i32) - rare.relic_eval.hp_credit;
        let growth_reward_count =
            (f.strategy_goal_count as i32) - rare.relic_eval.satisfied_priority;
        let forfeits_gold = (f.flags & FAST_FLAG_FORFEITS_GOLD) != 0;
        let max_hp_delta = if root.initial_player_max_hp > 0 {
            (root.initial_player_max_hp - (f.player_max_hp as i32))
                .clamp(i16::MIN as i32, i16::MAX as i32) as i16
        } else {
            0
        };
        let turn_i16 = cand.turn.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        let potion_count = cand.potion_count.min(u8::MAX as u16) as u8;
        let optional_paid_potion_count =
            cand.optional_paid_potion_count.min(u8::MAX as u16) as u8;
        let sig = cand
            .full_eval
            .as_ref()
            .map_or(0, |full| full.tactical_sig);
        let tactical_sig = if sig != 0 {
            sig
        } else {
            compute_tactical_signature(
                turn_i16,
                cand.used_potion_slot_mask,
                potion_count,
                f.death_save_use_count,
                f.alive_enemy_count,
                f.reviving_enemy_count,
                f.energy,
                f.cumulative_hp_lost as i32,
                f.recovered_hp as i32,
                k.best_hp as i32,
                f.enemy_hp,
                max_hp_delta,
                eff_def,
                k.optionality_score,
                f.playable_hand_count,
                k.setup_score as i32,
                k.board_investment as i32,
                max_dark_evoke,
                total_poison_and_doom,
                has_pure_growth,
                growth_hp_credit,
                growth_reward_count,
                f.alchemize_count,
                f.feed_count,
                has_relic,
                rare.relic_eval.satisfied_mask,
                rare.relic_eval.distance,
                rare.relic_eval.satisfied_priority,
                rare.relic_eval.hp_credit,
                preserve_theft,
                f.outstanding_stolen,
                forfeits_gold,
            )
        };
        ExtendedPruneEntry {
            effective_net_hp_loss_centi: eff_net_loss_centi,
            tactical_sig,
            node_id: id,
            primary: k.primary,
            aux_score: k.aux_score,
            forced_used_mask: cand.forced_used_mask,
            relic_satisfied_mask: rare.relic_eval.satisfied_mask,
            best_hp: k.best_hp as i32,
            effective_defense: eff_def,
            enemy_hp: f.enemy_hp,
            rem_enemies: k.rem_enemies as i32,
            board_investment: k.board_investment as i32,
            setup_score: k.setup_score as i32,
            max_dark_evoke,
            total_poison_and_doom,
            optionality_score: k.optionality_score,
            strategy_goal_hp_credit: f.strategy_goal_hp_credit as i32,
            relic_distance: rare.relic_eval.distance,
            relic_hp_credit: rare.relic_eval.hp_credit,
            relic_healing_hp_credit: rare.relic_eval.healing_hp_credit,
            relic_satisfied_priority: rare.relic_eval.satisfied_priority,
            growth_hp_credit,
            growth_reward_count,
            used_potion_slot_mask: cand.used_potion_slot_mask,
            potion_cost: k.potion_cost,
            strategic_hp_credit: f.strategic_hp_credit,
            energy: f.energy,
            outstanding_stolen: f.outstanding_stolen,
            playable_hand_count: f.playable_hand_count,
            optional_paid_potion_count,
            explicit_potion_count: k.explicit_potion_count,
            death_save_use_count: f.death_save_use_count,
            strategy_goal_count: f.strategy_goal_count,
            forfeits_gold,
            has_rollout_score: k.has_rollout_score(),
            has_choice_lookahead: k.has_choice_lookahead(),
        }
    }
}

static SEARCH_THREAD_POOLS: [OnceLock<rayon::ThreadPool>; 17] = [const { OnceLock::new() }; 17];
static PASS_RESOURCE_POOL: parking_lot::Mutex<Vec<PassResources>> =
    parking_lot::Mutex::new(Vec::new());
static EMPTY_STATIC_CTX: OnceLock<Arc<RootStaticContext>> = OnceLock::new();

fn empty_static_ctx() -> &'static Arc<RootStaticContext> {
    EMPTY_STATIC_CTX.get_or_init(|| Arc::new(RootStaticContext::default()))
}

fn shared_search_pool(dop: usize) -> Option<&'static rayon::ThreadPool> {
    if dop <= 1 {
        return None;
    }
    let idx = dop.min(16);
    Some(SEARCH_THREAD_POOLS[idx].get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(idx)
            .build()
            .expect("failed to build search thread pool")
    }))
}

#[inline]
fn reset_scratch_battle_state(s: &mut BattleState, static_ctx: &Arc<RootStaticContext>) {
    s.static_ctx = Arc::clone(static_ctx);
    s.hook_cache.clear();
    s.deck_dirty.0 = true;
    s.relic_dirty_mask = RelicDirtyMask(u64::MAX);
    s.handle_layout = Default::default();
    s.card_meta_dirty = sts2_kernel::state::CardMetaDirtyMask::ALL_DIRTY;
    s.extra_clutter_hint = sts2_kernel::state::ExtraClutterHintFlag(true);
    s.eot_card_cleanup_dirty = sts2_kernel::state::EotCleanupDirtyFlag(true);
}

struct PooledPassResources(Option<PassResources>);

impl Deref for PooledPassResources {
    type Target = PassResources;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        self.0.as_ref().expect("PassResources already dropped")
    }
}

impl DerefMut for PooledPassResources {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.as_mut().expect("PassResources already dropped")
    }
}

impl Drop for PooledPassResources {
    fn drop(&mut self) {
        if let Some(mut res) = self.0.take() {
            res.reset_for_new_solve(empty_static_ctx());
            res.trim_for_idle();
            let mut pool = PASS_RESOURCE_POOL.lock();
            if pool.is_empty() {
                pool.push(res);
            }
        }
    }
}

impl PassResources {
    fn acquire(
        static_ctx: &Arc<RootStaticContext>,
        worker_count: usize,
        transposition_entry_limit: usize,
    ) -> PooledPassResources {
        let pooled = {
            let mut pool = PASS_RESOURCE_POOL.lock();
            pool.pop()
        };
        let mut res = match pooled {
            Some(mut r) => {
                while r.scratches.len() < worker_count {
                    r.scratches.push(WorkerScratch::new(static_ctx.clone()));
                }
                while r.prev_worker_slabs.len() < r.scratches.len() {
                    r.prev_worker_slabs.push(LocalStateSlab::default());
                }
                while r.parent_states.len() < worker_count {
                    r.parent_states.push(empty_scratch_state(static_ctx.clone()));
                }
                while r.enumerated_tasks.len() < worker_count {
                    r.enumerated_tasks.push(Vec::new());
                }
                while r.rollout_workspaces.len() < worker_count {
                    r.rollout_workspaces
                        .push(crate::rollout::RolloutWorkspace::new(static_ctx.clone()));
                }
                r.tt.set_entry_limit(transposition_entry_limit.max(65_536));
                r.choice_probe_cache.clear();
                r.turn_rollout_cache.clear();
                r.reset_for_new_solve(static_ctx);
                r
            }
            None => Self::new(static_ctx, worker_count, transposition_entry_limit),
        };
        let _ = &mut res;
        PooledPassResources(Some(res))
    }

    fn reset_for_new_solve(&mut self, static_ctx: &Arc<RootStaticContext>) {
        self.reset_for_pass();
        for s in &mut self.scratches {
            s.reset_for_wave();
            reset_scratch_battle_state(&mut s.state, static_ctx);
            reset_scratch_battle_state(&mut s.pre_end_turn_state, static_ctx);
            for ps in &mut s.choice_paused_states {
                reset_scratch_battle_state(ps, static_ctx);
            }
            reset_scratch_battle_state(&mut s.choice_probe_ws.probe_state, static_ctx);
        }
        for ps in &mut self.parent_states {
            reset_scratch_battle_state(ps, static_ctx);
        }
        for rw in &mut self.rollout_workspaces {
            reset_scratch_battle_state(&mut rw.base_state, static_ctx);
            reset_scratch_battle_state(&mut rw.pilot_state, static_ctx);
        }
    }

    fn trim_for_idle(&mut self) {
        self.step_arena.trim_for_idle();
        self.turn_staging_arena.trim_for_idle();
        self.node_arena.trim_for_idle();
        for slab in &mut self.prev_worker_slabs {
            slab.trim_for_idle();
        }
        for s in &mut self.scratches {
            s.slab.trim_for_idle();
            s.choice_slab.trim_for_idle();
        }
        self.tt.clear_and_shrink_idle();
    }

    fn new(
        static_ctx: &Arc<RootStaticContext>,
        worker_count: usize,
        transposition_entry_limit: usize,
    ) -> Self {
        Self {
            scratches: (0..worker_count)
                .map(|_| WorkerScratch::new(static_ctx.clone()))
                .collect(),
            parent_states: (0..worker_count)
                .map(|_| empty_scratch_state(static_ctx.clone()))
                .collect(),
            enumerated_tasks: (0..worker_count).map(|_| Vec::new()).collect(),
            rollout_workspaces: (0..worker_count)
                .map(|_| crate::rollout::RolloutWorkspace::new(static_ctx.clone()))
                .collect(),
            node_arena: NodeArena::with_capacity(32_768, 32_768),
            prune_scratch: PruneScratch::default(),
            flat_tasks: Vec::with_capacity(128),
            deferred_batches: Vec::new(),
            probe_jobs: Vec::new(),
            step_arena: MainStateArena::default(),
            prev_worker_slabs: (0..worker_count).map(|_| LocalStateSlab::default()).collect(),
            turn_staging_arena: MainStateArena::default(),
            tt: ShardedParetoTT::new(transposition_entry_limit.max(65_536)),
            choice_probe_cache: ChoiceProbeCache::new(),
            turn_rollout_cache: TurnRolloutCache::new(),
            stand_pat_seen: hashbrown::HashSet::with_hasher(ahash::RandomState::with_seeds(
                0x3141_5926_5358_9793,
                0x2718_2818_2845_9045,
                0x1618_0339_8874_9895,
                0x1414_2135_6237_3095,
            )),
            sort_list: Vec::with_capacity(2048),
            parent_slices: (0..128).map(|_| AtomicU64::new(0)).collect(),
            active_parent_slices: Vec::with_capacity(128),
            tt_group_accepted: Box::new(std::array::from_fn(|_| Vec::with_capacity(64))),
            tt_accepted_epoch: vec![0u8; 512],
            tt_cur_epoch: 0,
            tt_shard_buckets: (0..TT_SHARD_COUNT).map(|_| Vec::with_capacity(8)).collect(),
            tt_accepted_flags: (0..512).map(|_| AtomicU8::new(0)).collect(),
            tt_flat_refs: Vec::with_capacity(512),
            accepted_mid_turn: Vec::with_capacity(512),
            mid_turn_ords: Vec::with_capacity(512),
            surviving_ords: Vec::with_capacity(256),
            surviving_commit_order: Vec::with_capacity(256),
            rollout_results: Vec::with_capacity(128),
        }
    }

    fn reset_for_pass(&mut self) {
        for s in &mut self.scratches {
            s.eval_intent_cache.clear();
            s.choice_probe_ws.intent_cache.clear();
            s.fatal_error = None;
            s.deferred_batches.clear();
            s.deferred_probe_out.clear();
        }
        for rw in &mut self.rollout_workspaces {
            rw.intent_cache.clear();
        }
        self.node_arena.clear();
        self.flat_tasks.clear();
        self.deferred_batches.clear();
        self.probe_jobs.clear();
        for et in &mut self.enumerated_tasks {
            et.clear();
        }
        self.step_arena.clear();
        for slab in &mut self.prev_worker_slabs {
            slab.clear();
        }
        self.turn_staging_arena.clear();
        self.tt.reset_generation();
        self.tt.branches_pruned = 0;
        self.stand_pat_seen.clear();
        self.sort_list.clear();
        self.active_parent_slices.clear();
        for b in &mut self.tt_shard_buckets {
            b.clear();
        }
        self.tt_flat_refs.clear();
        self.accepted_mid_turn.clear();
        self.mid_turn_ords.clear();
        self.surviving_ords.clear();
        self.surviving_commit_order.clear();
        self.rollout_results.clear();
    }
}

#[cold]
#[inline(never)]
fn clone_cow_str_owned_cold(src: &Cow<'static, str>, dst: &mut Cow<'static, str>) {
    match src {
        Cow::Borrowed(sb) => *dst = Cow::Borrowed(sb),
        Cow::Owned(ss) => match dst {
            Cow::Owned(ds) => {
                if ds != ss {
                    ds.clone_from(ss);
                }
            }
            Cow::Borrowed(_) => *dst = src.clone(),
        },
    }
}

#[inline(always)]
fn clone_cow_str_into(src: &Cow<'static, str>, dst: &mut Cow<'static, str>) {
    if let (Cow::Borrowed(sb), Cow::Borrowed(db)) = (src, &mut *dst) {
        if db.as_ptr() != sb.as_ptr() || db.len() != sb.len() {
            *db = sb;
        }
        return;
    }
    clone_cow_str_owned_cold(src, dst);
}

#[inline(always)]
fn copy_smallvec_copy<A: smallvec::Array>(src: &SmallVec<A>, dst: &mut SmallVec<A>)
where
    A::Item: Copy,
{
    let sl = src.len();
    if sl == 0 {
        if !dst.is_empty() {
            dst.clear();
        }
        return;
    }
    if dst.len() == sl {
        dst.as_mut_slice().copy_from_slice(src.as_slice());
        return;
    }
    dst.clear();
    dst.extend_from_slice(src.as_slice());
}

#[inline(always)]
fn copy_vec_copy<T: Copy>(src: &[T], dst: &mut Vec<T>) {
    let sl = src.len();
    if sl == 0 {
        if !dst.is_empty() {
            dst.clear();
        }
        return;
    }
    if dst.capacity() >= sl {
        // SAFETY: `dst.capacity() >= sl` and `T: Copy` has no Drop glue; elements in `0..sl` are initialized.
        unsafe {
            std::ptr::copy_nonoverlapping(src.as_ptr(), dst.as_mut_ptr(), sl);
            dst.set_len(sl);
        }
        return;
    }
    dst.clear();
    dst.extend_from_slice(src);
}

#[cold]
#[inline(never)]
fn clone_opt_cow_owned_cold(src: &Option<Cow<'static, str>>, dst: &mut Option<Cow<'static, str>>) {
    *dst = src.clone();
}

#[cold]
#[inline(never)]
fn clear_opt_cow_owned_cold(dst: &mut Option<Cow<'static, str>>) {
    *dst = None;
}

#[inline(always)]
fn clone_opt_cow_str_into(
    src: &Option<Cow<'static, str>>,
    dst: &mut Option<Cow<'static, str>>,
) {
    if let (None, None) = (&*dst, src) {
        return;
    }
    if let (Some(Cow::Borrowed(db)), Some(Cow::Borrowed(sb))) = (&mut *dst, src) {
        if db.as_ptr() != sb.as_ptr() || db.len() != sb.len() {
            *db = sb;
        }
        return;
    }
    match (dst, src) {
        (Some(dc), Some(sc)) => clone_cow_str_into(sc, dc),
        (dst_slot, Some(Cow::Borrowed(sb))) => match dst_slot {
            // SAFETY: `None` has no destructor drop glue.
            None => unsafe {
                std::ptr::write(dst_slot, Some(Cow::Borrowed(sb)));
            },
            Some(_) => unreachable!(),
        },
        (dst_slot, Some(_)) => clone_opt_cow_owned_cold(src, dst_slot),
        (dst_slot, None) => match dst_slot {
            None => {}
            // SAFETY: `Some(Cow::Borrowed(_))` has no destructor drop glue.
            Some(Cow::Borrowed(_)) => unsafe {
                std::ptr::write(dst_slot, None);
            },
            Some(Cow::Owned(_)) => clear_opt_cow_owned_cold(dst_slot),
        },
    }
}

#[inline(always)]
fn clone_small_cow_strings_into(
    src: &[Cow<'static, str>],
    dst: &mut Vec<Cow<'static, str>>,
) {
    let sl = src.len();
    if sl == 0 {
        if !dst.is_empty() {
            dst.clear();
        }
        return;
    }
    let dl = dst.len();
    if dl == sl {
        for (dc, sc) in dst.iter_mut().zip(src.iter()) {
            if let (Cow::Borrowed(db), Cow::Borrowed(sb)) = (&*dc, sc) {
                if db.as_ptr() == sb.as_ptr() && db.len() == sb.len() {
                    continue;
                }
            }
            clone_cow_str_into(sc, dc);
        }
        return;
    }
    let keep = dl.min(sl);
    for (dc, sc) in dst[..keep].iter_mut().zip(src[..keep].iter()) {
        if let (Cow::Borrowed(db), Cow::Borrowed(sb)) = (&*dc, sc) {
            if db.as_ptr() == sb.as_ptr() && db.len() == sb.len() {
                continue;
            }
        }
        clone_cow_str_into(sc, dc);
    }
    if sl > keep {
        dst.reserve(sl - keep);
        for s in &src[keep..] {
            match s {
                Cow::Borrowed(sb) => dst.push(Cow::Borrowed(sb)),
                Cow::Owned(_) => dst.push(s.clone()),
            }
        }
    } else if dl > keep {
        dst.truncate(keep);
    }
}

#[inline]
fn clone_field_map_into(src: &FieldMap, dst: &mut FieldMap) {
    src.clone_into_map(dst);
}

#[inline]
fn clone_semantic_history_into(src: &SemanticHistory, dst: &mut SemanticHistory) {
    copy_vec_copy(&src.exhausts, &mut dst.exhausts);
    copy_vec_copy(&src.card_block_gains, &mut dst.card_block_gains);
    copy_vec_copy(&src.orb_channels, &mut dst.orb_channels);
    copy_vec_copy(&src.energy_spent, &mut dst.energy_spent);
    copy_vec_copy(&src.card_drawn, &mut dst.card_drawn);

    if src.actor_events.is_empty() {
        if !dst.actor_events.is_empty() {
            dst.actor_events.clear();
        }
    } else {
        let keep = dst.actor_events.len().min(src.actor_events.len());
        for i in 0..keep {
            clone_cow_str_into(&src.actor_events[i].0, &mut dst.actor_events[i].0);
            dst.actor_events[i].1 = src.actor_events[i].1;
            dst.actor_events[i].2 = src.actor_events[i].2;
        }
        if src.actor_events.len() > keep {
            dst.actor_events.reserve(src.actor_events.len() - keep);
            for item in &src.actor_events[keep..] {
                dst.actor_events.push(item.clone());
            }
        } else if dst.actor_events.len() > keep {
            dst.actor_events.truncate(keep);
        }
    }

    dst.growth_rewards = src.growth_rewards;
    dst.brightest_flame_max_hp_spent = src.brightest_flame_max_hp_spent;
    dst.prior_player_unblocked_hits = src.prior_player_unblocked_hits;
    dst.prior_player_damaged_turn = src.prior_player_damaged_turn;
    dst.prior_exhaust_count = src.prior_exhaust_count;
    dst.turn_cache = src.turn_cache;
}

#[inline]
fn clone_hook_cache_into(src: &ActiveHookCache, dst: &mut ActiveHookCache) {
    let prev_dst_dirty = dst.dirty_mask;
    dst.dirty_mask = src.dirty_mask;
    if src.dirty_mask == HOOK_DIRTY_ALL {
        return;
    }
    if prev_dst_dirty == 0 && src.dirty_mask == 0 && src.revision != 0 && dst.revision == src.revision {
        #[cfg(debug_assertions)]
        debug_assert!(dst.exact_eq(src), "ActiveHookCache revision hit diverged");
        return;
    }
    dst.masks = src.masks;
    dst.in_combat_mask = src.in_combat_mask;
    dst.in_combat_overflow = src.in_combat_overflow;
    dst.combat_has_player = src.combat_has_player;
    dst.has_active_history_course_relic = src.has_active_history_course_relic;
    dst.living_check_player_idx = src.living_check_player_idx;
    dst.primary_enemy_indices = src.primary_enemy_indices;
    dst.primary_enemy_count = src.primary_enemy_count;
    dst.enemy_power_flags = src.enemy_power_flags;
    dst.flat_slot_words = src.flat_slot_words;
    dst.revision = src.revision;
    dst.cached_tables = src.cached_tables;
    dst.listener_dominion = src.listener_dominion;
    copy_vec_copy(&src.listener_pre_player, &mut dst.listener_pre_player);
    copy_vec_copy(&src.listener_post_player, &mut dst.listener_post_player);
    copy_vec_copy(&src.listener_relics, &mut dst.listener_relics);
    copy_vec_copy(&src.listener_potions, &mut dst.listener_potions);
    copy_vec_copy(&src.listener_orbs, &mut dst.listener_orbs);
    copy_vec_copy(&src.listener_hand, &mut dst.listener_hand);
    copy_vec_copy(&src.listener_draw, &mut dst.listener_draw);
    copy_vec_copy(&src.listener_discard, &mut dst.listener_discard);
    copy_vec_copy(&src.listener_exhaust, &mut dst.listener_exhaust);
    copy_vec_copy(&src.listener_play, &mut dst.listener_play);
    if dst.listener_deck != src.listener_deck {
        copy_vec_copy(&src.listener_deck, &mut dst.listener_deck);
    }
    copy_vec_copy(&src.listener_run_models, &mut dst.listener_run_models);
    if src.flat_slot_words != 0 {
        let dst_masks = dst
            .slot_listener_masks
            .get_or_insert_with(|| Box::new([0u64; sts2_kernel::state::HOOK_SLOT_TABLE_LEN]));
        if let Some(src_masks) = src.slot_listener_masks.as_deref() {
            dst_masks.copy_from_slice(src_masks);
        }
        if src.flat_slot_words >= 2 {
            let dst_ext = dst.slot_listener_masks_ext.get_or_insert_with(|| {
                Box::new([[0u64; sts2_kernel::state::HOOK_SLOT_TABLE_LEN]; 3])
            });
            if let Some(src_ext) = src.slot_listener_masks_ext.as_deref() {
                let active_planes = (src.flat_slot_words as usize - 1).min(3);
                for p in 0..active_planes {
                    dst_ext[p].copy_from_slice(&src_ext[p]);
                }
            }
        }
        if !dst.flat_masks_w0.is_empty() {
            dst.flat_masks_w0.clear();
            dst.flat_masks_w1.clear();
            dst.flat_masks_w2.clear();
        }
    } else {
        copy_vec_copy(&src.flat_masks_w0, &mut dst.flat_masks_w0);
        copy_vec_copy(&src.flat_masks_w1, &mut dst.flat_masks_w1);
        copy_vec_copy(&src.flat_masks_w2, &mut dst.flat_masks_w2);
    }
    copy_vec_copy(&src.flat_listeners, &mut dst.flat_listeners);
}

#[inline]
pub(crate) fn clone_battle_state_into(src: &BattleState, dst: &mut BattleState) {
    let same_root = Arc::ptr_eq(&dst.static_ctx, &src.static_ctx);
    let prefix_len = src.static_ctx.root_deck_prefix_len;
    let skip_immutable_deck = same_root
        && !src.deck_dirty.0
        && !dst.deck_dirty.0
        && prefix_len > 0
        && dst.cards.len() >= prefix_len
        && src.cards.len() >= prefix_len;
    if !same_root {
        dst.static_ctx = Arc::clone(&src.static_ctx);
    }
    dst.current_side = src.current_side;
    dst.round_number = src.round_number;
    dst.is_in_progress = src.is_in_progress;
    dst.is_starting = src.is_starting;
    dst.won = src.won;
    dst.is_player_extra_turn = src.is_player_extra_turn;
    dst.gold_was_stolen = src.gold_was_stolen;
    dst.battleworn_dummy_timed_out = src.battleworn_dummy_timed_out;
    dst.end_player_turn_requested = src.end_player_turn_requested;
    dst.card_action_boundary_depth = src.card_action_boundary_depth;
    dst.next_creature_id = src.next_creature_id;
    dst.initial_encounter_monster_count = src.initial_encounter_monster_count;
    dst.next_card_handle = src.next_card_handle;
    dst.next_creature_handle = src.next_creature_handle;
    dst.next_power_handle = src.next_power_handle;
    dst.next_attack_id = src.next_attack_id;
    dst.run_rngs.seed = src.run_rngs.seed;
    dst.run_rngs.streams = src.run_rngs.streams;
    if !Arc::ptr_eq(&dst.run_rngs.string_seed, &src.run_rngs.string_seed) {
        dst.run_rngs.string_seed = Arc::clone(&src.run_rngs.string_seed);
    }
    dst.player_rngs = src.player_rngs;

    // Copy PlayerInst with in-place map/vec reuse
    let sp = &src.player;
    let dp = &mut dst.player;
    dp.creature_handle = sp.creature_handle;
    dp.current_hp = sp.current_hp;
    dp.max_hp = sp.max_hp;
    dp.block = sp.block;
    dp.cumulative_hp_lost = sp.cumulative_hp_lost;
    dp.gold = sp.gold;
    dp.max_energy = sp.max_energy;
    dp.base_orb_slot_count = sp.base_orb_slot_count;
    dp.is_active_for_hooks = sp.is_active_for_hooks;
    dp.can_use_or_remove_potions = sp.can_use_or_remove_potions;
    dp.energy = sp.energy;
    dp.stars = sp.stars;
    dp.phase = sp.phase;
    dp.turn_number = sp.turn_number;
    dp.counters = sp.counters;
    copy_vec_copy(&sp.hand, &mut dp.hand);
    copy_vec_copy(&sp.draw, &mut dp.draw);
    copy_vec_copy(&sp.discard, &mut dp.discard);
    copy_vec_copy(&sp.exhaust, &mut dp.exhaust);
    copy_vec_copy(&sp.play, &mut dp.play);
    if skip_immutable_deck && dp.deck.len() == sp.deck.len() {
        #[cfg(debug_assertions)]
        debug_assert_eq!(dp.deck.as_slice(), sp.deck.as_slice());
    } else {
        copy_vec_copy(&sp.deck, &mut dp.deck);
    }
    copy_smallvec_copy(&sp.potion_slots, &mut dp.potion_slots);
    dp.combat_generated_potion_mask = sp.combat_generated_potion_mask;
    dp.orb_capacity = sp.orb_capacity;
    copy_vec_copy(&sp.orbs, &mut dp.orbs);
    copy_vec_copy(&sp.arrows, &mut dp.arrows);
    dp.card_or_potion_effect_depth = sp.card_or_potion_effect_depth;
    dp.death_save_use_count = sp.death_save_use_count;
    dp.death_save_relic_hp_restored = sp.death_save_relic_hp_restored;
    dp.death_save_potion_hp_restored = sp.death_save_potion_hp_restored;
    if same_root
        && src.static_ctx.has_root_relics
        && dp.relics.len() == sp.relics.len()
        && sp.relics.len() <= 64
    {
        let valid_mask = if sp.relics.len() == 64 {
            u64::MAX
        } else {
            (1u64 << sp.relics.len()) - 1
        };
        let mut dirty = (src.relic_dirty_mask.0 | dst.relic_dirty_mask.0) & valid_mask;
        while dirty != 0 {
            let i = dirty.trailing_zeros() as usize;
            dirty &= dirty - 1;
            let sr = &sp.relics[i];
            let dr = &mut dp.relics[i];
            dr.id = sr.id;
            dr.is_wax = sr.is_wax;
            dr.is_melted = sr.is_melted;
            dr.stack_count = sr.stack_count;
            dr.floor_added_to_deck = sr.floor_added_to_deck;
            if !sr.private_fields.is_empty() || !dr.private_fields.is_empty() {
                clone_field_map_into(&sr.private_fields, &mut dr.private_fields);
            }
        }
        #[cfg(debug_assertions)]
        debug_assert_eq!(dp.relics, sp.relics);
    } else {
        let relic_keep = dp.relics.len().min(sp.relics.len());
        for (dr, sr) in dp.relics[..relic_keep].iter_mut().zip(sp.relics[..relic_keep].iter()) {
            dr.id = sr.id;
            dr.is_wax = sr.is_wax;
            dr.is_melted = sr.is_melted;
            dr.stack_count = sr.stack_count;
            dr.floor_added_to_deck = sr.floor_added_to_deck;
            if !sr.private_fields.is_empty() || !dr.private_fields.is_empty() {
                clone_field_map_into(&sr.private_fields, &mut dr.private_fields);
            }
        }
        if sp.relics.len() > relic_keep {
            dp.relics.reserve(sp.relics.len() - relic_keep);
            for sr in &sp.relics[relic_keep..] {
                dp.relics.push(sr.clone());
            }
        } else if dp.relics.len() > relic_keep {
            dp.relics.truncate(sp.relics.len());
        }
    }

    if skip_immutable_deck {
        #[cfg(debug_assertions)]
        debug_assert_eq!(&dst.cards[..prefix_len], &src.cards[..prefix_len]);
        let tail_src = &src.cards[prefix_len..];
        let total_cards = src.cards.len();
        if dst.cards.capacity() >= total_cards {
            // SAFETY: `dst.cards[..prefix_len]` is preserved and `capacity >= total_cards`.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    tail_src.as_ptr(),
                    dst.cards.as_mut_ptr().add(prefix_len),
                    tail_src.len(),
                );
                dst.cards.set_len(total_cards);
            }
        } else {
            dst.cards.truncate(prefix_len);
            dst.cards.extend_from_slice(tail_src);
        }
    } else {
        copy_vec_copy(&src.cards, &mut dst.cards);
    }

    if src.card_extensions.is_empty() {
        dst.card_extensions.clear();
    } else {
        let ext_keep = dst.card_extensions.len().min(src.card_extensions.len());
        for (de, se) in dst.card_extensions[..ext_keep]
            .iter_mut()
            .zip(src.card_extensions[..ext_keep].iter())
        {
            copy_vec_copy(&se.local_energy_modifiers, &mut de.local_energy_modifiers);
            if se.enchantment.is_some() || de.enchantment.is_some() {
                match (&mut de.enchantment, &se.enchantment) {
                    (Some(de_e), Some(se_e)) => {
                        de_e.id = se_e.id;
                        de_e.amount = se_e.amount;
                        de_e.status = se_e.status;
                        if !se_e.private_fields.is_empty() || !de_e.private_fields.is_empty() {
                            clone_field_map_into(&se_e.private_fields, &mut de_e.private_fields);
                        }
                    }
                    _ => de.enchantment.clone_from(&se.enchantment),
                }
            }
            if !se.private_fields.is_empty() || !de.private_fields.is_empty() {
                clone_field_map_into(&se.private_fields, &mut de.private_fields);
            }
        }
        if src.card_extensions.len() > ext_keep {
            dst.card_extensions
                .reserve(src.card_extensions.len() - ext_keep);
            for se in &src.card_extensions[ext_keep..] {
                dst.card_extensions.push(se.clone());
            }
        } else if dst.card_extensions.len() > ext_keep {
            dst.card_extensions.truncate(src.card_extensions.len());
        }
    }

    let cr_keep = dst.creatures.len().min(src.creatures.len());
    for (dc, sc) in dst.creatures[..cr_keep]
        .iter_mut()
        .zip(src.creatures[..cr_keep].iter())
    {
        let prev_c_handle = dc.handle;
        dc.handle = sc.handle;
        dc.combat_id = sc.combat_id;
        dc.side = sc.side;
        clone_opt_cow_str_into(&sc.slot_name, &mut dc.slot_name);
        dc.is_player = sc.is_player;
        dc.pet_owner_player_index = sc.pet_owner_player_index;
        dc.monster_id = sc.monster_id;
        dc.current_hp = sc.current_hp;
        dc.max_hp = sc.max_hp;
        dc.block = sc.block;
        dc.cumulative_hp_lost = sc.cumulative_hp_lost;
        dc.is_detached = sc.is_detached;
        dc.has_minion_power = sc.has_minion_power;
        copy_vec_copy(&sc.powers, &mut dc.powers);
        match (&mut dc.monster, &sc.monster) {
            (Some(dm), Some(sm)) => {
                let prev_mid = dm.monster_id;
                dm.monster_id = sm.monster_id;
                dm.rng = sm.rng;
                dm.is_performing_move = sm.is_performing_move;
                dm.spawned_this_turn = sm.spawned_this_turn;
                clone_cow_str_into(&sm.current_state_id, &mut dm.current_state_id);
                clone_opt_cow_str_into(&sm.next_move_id, &mut dm.next_move_id);
                dm.next_move_can_transition_away = sm.next_move_can_transition_away;
                dm.performed_first_move = sm.performed_first_move;
                clone_small_cow_strings_into(&sm.state_log_ids, &mut dm.state_log_ids);
                clone_opt_cow_str_into(
                    &sm.transient_move_follow_up_state_id,
                    &mut dm.transient_move_follow_up_state_id,
                );
                if !sm.setup_fields.is_empty() || !dm.setup_fields.is_empty() {
                    let skip_setup = same_root
                        && prev_c_handle == sc.handle
                        && prev_mid == sm.monster_id
                        && dm.setup_fields.len() == sm.setup_fields.len();
                    if skip_setup {
                        #[cfg(debug_assertions)]
                        debug_assert_eq!(dm.setup_fields, sm.setup_fields);
                    } else {
                        clone_field_map_into(&sm.setup_fields, &mut dm.setup_fields);
                    }
                }
                if !sm.private_fields.is_empty() || !dm.private_fields.is_empty() {
                    clone_field_map_into(&sm.private_fields, &mut dm.private_fields);
                }
            }
            _ => dc.monster.clone_from(&sc.monster),
        }
    }
    if src.creatures.len() > cr_keep {
        dst.creatures.reserve(src.creatures.len() - cr_keep);
        for sc in &src.creatures[cr_keep..] {
            dst.creatures.push(sc.clone());
        }
    } else if dst.creatures.len() > cr_keep {
        dst.creatures.truncate(src.creatures.len());
    }

    if src.power_extensions.is_empty() {
        dst.power_extensions.clear();
    } else {
        let pe_keep = dst.power_extensions.len().min(src.power_extensions.len());
        for (dp, sp) in dst.power_extensions[..pe_keep]
            .iter_mut()
            .zip(src.power_extensions[..pe_keep].iter())
        {
            dp.attack_command_id = sp.attack_command_id;
            dp.amount_when_attack_started = sp.amount_when_attack_started;
            if !sp.impaled_arrows.is_empty() || !dp.impaled_arrows.is_empty() {
                copy_smallvec_copy(&sp.impaled_arrows, &mut dp.impaled_arrows);
            }
            dp.hit_counter = sp.hit_counter;
            if !sp.private_fields.is_empty() || !dp.private_fields.is_empty() {
                clone_field_map_into(&sp.private_fields, &mut dp.private_fields);
            }
        }
        if src.power_extensions.len() > pe_keep {
            dst.power_extensions
                .reserve(src.power_extensions.len() - pe_keep);
            for sp in &src.power_extensions[pe_keep..] {
                dst.power_extensions.push(sp.clone());
            }
        } else if dst.power_extensions.len() > pe_keep {
            dst.power_extensions.truncate(src.power_extensions.len());
        }
    }

    copy_vec_copy(&src.ally_handles, &mut dst.ally_handles);
    copy_vec_copy(&src.enemy_handles, &mut dst.enemy_handles);
    copy_smallvec_copy(&src.escaped_handles, &mut dst.escaped_handles);
    copy_smallvec_copy(&src.removed_handles, &mut dst.removed_handles);
    copy_smallvec_copy(&src.spawned_enemy_handles, &mut dst.spawned_enemy_handles);
    copy_smallvec_copy(&src.pet_handles, &mut dst.pet_handles);
    copy_vec_copy(&src.damage_history, &mut dst.damage_history);
    clone_semantic_history_into(&src.semantic_history, &mut dst.semantic_history);
    if src.pending.is_none() {
        if dst.pending.is_some() {
            dst.pending = None;
        }
    } else {
        dst.pending.clone_from(&src.pending);
    }
    dst.vakuu_auto_select_depth = src.vakuu_auto_select_depth;
    clone_hook_cache_into(&src.hook_cache, &mut dst.hook_cache);
    dst.deck_dirty = src.deck_dirty;
    dst.relic_dirty_mask = src.relic_dirty_mask;
    dst.handle_layout = src.handle_layout;
    dst.card_meta_dirty = src.card_meta_dirty;
    dst.extra_clutter_hint = src.extra_clutter_hint;
    dst.eot_card_cleanup_dirty = src.eot_card_cleanup_dirty;
}

struct BeamRunOutput {
    best_node: Option<NodeId>,
    best_turn: i32,
    nodes_in_arena: usize,
    root_baseline: RootEvalBaseline,
    won: bool,
    survives: bool,
    is_event_defeat: bool,
    death_save_use_count: i32,
    player_hp: i32,
    end_player_max_hp: i32,
    enemy_hp: i32,
    all_forced_uses_satisfied: bool,
    all_strategy_targets_satisfied: bool,
    strategic_hp_deficit: i32,
    net_persistent_hp_delta_centi: i32,
    net_potion_opportunity_cost_centi: i32,
    strategy_goal_hp_credit: i32,
    strategy_goal_count: i32,
    growth_rewards: GrowthCounters,
    relic_eval: RelicCounterEvaluation,
    recovered_hp: i32,
    hp_lost: i32,
    potion_count: usize,
    explicit_potion_count: usize,
    optional_potion_count: usize,
    optional_paid_potion_count: usize,
    optional_potion_strategic_cost: i32,
    raw_optional_paid_potion_cost: i32,
    optional_ambergris_count: i32,
    used_potion_slot_mask: u16,
    alchemize_count: u8,
    empty_potion_slots: u8,
    outstanding_stolen: i32,
    forfeits_gold: bool,
    has_completed_turn_ongoing: bool,
    completed_ongoing_turn: i32,
    score: i64,
    actions: Vec<PlannedAction>,
    expanded_nodes: usize,
    transition_count: u64,
    choice_replay_attempts: u64,
    tt_pruned: u64,
    incumbent_pruned: u64,
    por_pruned: u64,
    packed_state_bytes_total: u64,
    termination: &'static str,
    was_losslessly_exhausted: bool,
    terminal_cards_hint: Vec<sts2_types::CardId>,
    early_turn_frontiers: Vec<crate::early_turn::EarlyTurnFrontierCandidate>,
    boss_tempo_prefixes: Vec<BossTempoPrefix>,
}

impl BeamRunOutput {
    fn clone_summary(&self) -> Self {
        Self {
            best_node: self.best_node,
            best_turn: self.best_turn,
            nodes_in_arena: self.nodes_in_arena,
            root_baseline: self.root_baseline.clone(),
            won: self.won,
            survives: self.survives,
            is_event_defeat: self.is_event_defeat,
            death_save_use_count: self.death_save_use_count,
            player_hp: self.player_hp,
            end_player_max_hp: self.end_player_max_hp,
            enemy_hp: self.enemy_hp,
            all_forced_uses_satisfied: self.all_forced_uses_satisfied,
            all_strategy_targets_satisfied: self.all_strategy_targets_satisfied,
            strategic_hp_deficit: self.strategic_hp_deficit,
            net_persistent_hp_delta_centi: self.net_persistent_hp_delta_centi,
            net_potion_opportunity_cost_centi: self.net_potion_opportunity_cost_centi,
            strategy_goal_hp_credit: self.strategy_goal_hp_credit,
            strategy_goal_count: self.strategy_goal_count,
            growth_rewards: self.growth_rewards,
            relic_eval: self.relic_eval,
            recovered_hp: self.recovered_hp,
            hp_lost: self.hp_lost,
            potion_count: self.potion_count,
            explicit_potion_count: self.explicit_potion_count,
            optional_potion_count: self.optional_potion_count,
            optional_paid_potion_count: self.optional_paid_potion_count,
            optional_potion_strategic_cost: self.optional_potion_strategic_cost,
            raw_optional_paid_potion_cost: self.raw_optional_paid_potion_cost,
            optional_ambergris_count: self.optional_ambergris_count,
            used_potion_slot_mask: self.used_potion_slot_mask,
            alchemize_count: self.alchemize_count,
            empty_potion_slots: self.empty_potion_slots,
            outstanding_stolen: self.outstanding_stolen,
            forfeits_gold: self.forfeits_gold,
            has_completed_turn_ongoing: self.has_completed_turn_ongoing,
            completed_ongoing_turn: self.completed_ongoing_turn,
            score: self.score,
            actions: self.actions.clone(),
            expanded_nodes: self.expanded_nodes,
            transition_count: self.transition_count,
            choice_replay_attempts: self.choice_replay_attempts,
            tt_pruned: self.tt_pruned,
            incumbent_pruned: self.incumbent_pruned,
            por_pruned: self.por_pruned,
            packed_state_bytes_total: self.packed_state_bytes_total,
            termination: self.termination,
            was_losslessly_exhausted: self.was_losslessly_exhausted,
            terminal_cards_hint: self.terminal_cards_hint.clone(),
            early_turn_frontiers: Vec::new(),
            boss_tempo_prefixes: Vec::new(),
        }
    }
}

fn target_candidates_for(
    state: &BattleState,
    target_type: TargetType,
) -> SmallVec<[Option<CreatureHandle>; 8]> {
    let mut out = SmallVec::new();
    match target_type {
        TargetType::AnyEnemy => {
            for &eh in &state.enemy_handles {
                if state.creature(eh).is_some_and(|c| c.is_alive()) && state.contains_creature(eh) {
                    out.push(Some(eh));
                }
            }
        }
        TargetType::AnyAlly => {
            for &ah in &state.ally_handles {
                if ah != state.player.creature_handle
                    && state.creature(ah).is_some_and(|c| c.is_alive())
                {
                    out.push(Some(ah));
                }
            }
        }
        TargetType::AnyPlayer => {
            if state.player_creature().is_some_and(|c| c.is_alive()) {
                out.push(Some(state.player.creature_handle));
            }
        }
        _ => out.push(None),
    }
    out
}

#[inline]
fn move_name_is_summoner(name: &str) -> bool {
    let bytes = name.as_bytes();
    for needle in [b"SUMMON".as_slice(), b"SPAWN", b"REVIVE", b"REBIRTH"] {
        if bytes.len() >= needle.len()
            && bytes.windows(needle.len()).any(|w| w.eq_ignore_ascii_case(needle))
        {
            return true;
        }
    }
    false
}

/// Priority-ranked card/target selection matching `HotParallelExpander.BuildDistinctCardFirstMask`:
/// ranks enemy targets per attack (lethal -> summoner -> highest incoming -> lowest effective HP -> slot)
/// and prioritizes lethal / 0-cost / energy / draw / power / setup / block / debuff cards.
fn filter_play_candidates_priority(
    state: &mut BattleState,
    content: &dyn ContentDispatcher,
    hand_cards: &[CardHandle],
    play_candidates: &[(usize, usize, CardHandle, Option<CreatureHandle>, PorActionClass)],
    max_branches: usize,
    keep_mask: &mut [bool],
) {
    let enemy_count = state.enemy_handles.len().min(64);
    let mut enemy_eff_hp = [0i32; 64];
    let mut enemy_incoming = [0i32; 64];
    let mut enemy_is_summoner = [false; 64];
    let mut enemy_is_minion = [false; 64];
    let mut any_alive_minion = false;
    let mut min_alive_enemy_eff_hp = i32::MAX;

    for e in 0..enemy_count {
        let eh = state.enemy_handles[e];
        let Some(ec) = state.creature(eh) else {
            continue;
        };
        if !ec.is_alive() {
            continue;
        }
        let eff_hp = ec.current_hp.max(0) + ec.block.max(0);
        enemy_eff_hp[e] = eff_hp;
        if eff_hp > 0 && eff_hp < min_alive_enemy_eff_hp {
            min_alive_enemy_eff_hp = eff_hp;
        }
        if ec.has_minion_power
            || ec
                .powers
                .iter()
                .any(|p| p.id == PowerId::MinionPower && p.amount > 0)
        {
            enemy_is_minion[e] = true;
            any_alive_minion = true;
        }
        if let Ok(hits) = content.monster_attack_intent_hits(state, eh) {
            let mut dmg = 0i32;
            for (d, count) in hits {
                let di = d.to_i32_trunc().unwrap_or(0);
                if di > 0 && count > 0 {
                    dmg = dmg.saturating_add(di.saturating_mul(count));
                }
            }
            enemy_incoming[e] = dmg;
        }
        if let Some(m) = ec.monster.as_deref() {
            let move_str = m
                .next_move_id
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| m.current_state_id.as_ref());
            if !move_str.is_empty() && move_name_is_summoner(move_str) {
                enemy_is_summoner[e] = true;
            }
        }
    }
    if any_alive_minion {
        for e in 0..enemy_count {
            let eh = state.enemy_handles[e];
            if state.creature(eh).is_some_and(|c| c.is_alive()) && !enemy_is_minion[e] {
                enemy_is_summoner[e] = true;
            }
        }
    }

    let player_strength = state.player_creature().map_or(0, |pc| {
        pc.powers
            .iter()
            .find(|p| p.id == PowerId::StrengthPower)
            .map_or(0, |p| p.amount)
    });

    let mut card_est_dmg: SmallVec<[i32; 32]> = smallvec::smallvec![0; hand_cards.len()];
    let mut card_priority: SmallVec<[i32; 32]> = smallvec::smallvec![0; hand_cards.len()];
    let mut card_has_lethal: SmallVec<[bool; 32]> = smallvec::smallvec![false; hand_cards.len()];

    for (h_idx, &ch) in hand_cards.iter().enumerate() {
        let Some(card) = state.card(ch).copied() else {
            continue;
        };
        let Ok(meta) = content.card_meta(card.id) else {
            continue;
        };
        let eff_cost = sts2_kernel::ordering::resolve_energy_cost(state, content, ch)
            .unwrap_or(card.base_energy_cost as i32);
        let star_cost = sts2_kernel::ordering::resolve_star_cost(state, content, ch).unwrap_or(0);
        let rp = state.fast_card_rollout_profile(content, card.id);

        let mut prio = (40 - eff_cost.max(0) * 10).max(0);
        if eff_cost == 0 && star_cost <= 0 {
            prio += 400;
        }
        if meta.card_type == CardType::Power {
            prio += 250;
        }
        if rp.setup_weight > 0 {
            prio += 150.min(i32::from(rp.setup_weight) * 15);
        }
        if (rp.meta_flags & rollout_meta_flags::GAINS_BLOCK) != 0
            || (rp.op_flags & rollout_op_flags::HAS_BLOCK) != 0
            || meta.flags.contains(CardStaticFlags::GAINS_BLOCK)
            || meta.base_tags.contains(CardTags::DEFEND)
        {
            prio += 150;
        }
        if (rp.op_flags & rollout_op_flags::HAS_ENERGY_OR_STAR_GAIN) != 0 {
            prio += 350;
        }
        if (rp.op_flags & rollout_op_flags::HAS_DRAW) != 0 {
            prio += 300;
        }
        if (rp.meta_flags & rollout_meta_flags::APPLIES_WEAK) != 0
            || (rp.op_flags & rollout_op_flags::HAS_ENEMY_DEBUFF) != 0
        {
            prio += 120;
        }
        if meta.card_type == CardType::Attack {
            let base_dmg = if card.upgrade_level > 0 {
                rp.upg_damage
            } else {
                rp.base_damage
            } as i32;
            let hits = if card.upgrade_level > 0 {
                rp.upg_hits
            } else {
                rp.hits
            } as i32;
            let per_hit = (base_dmg + player_strength).max(0);
            card_est_dmg[h_idx] = if hits > 0 {
                per_hit.saturating_mul(hits)
            } else if base_dmg > 0 {
                per_hit
            } else {
                0
            };
        }
        card_priority[h_idx] = prio;
    }

    let cand_len = play_candidates.len();
    let mut target_keys: SmallVec<[i64; 128]> = smallvec::smallvec![0; cand_len];
    for (a, &(_, h_idx, _, target_handle, _)) in play_candidates.iter().enumerate() {
        let est_dmg = card_est_dmg[h_idx];
        if let Some(th) = target_handle {
            if let Some(t_slot) = state.enemy_handles[..enemy_count]
                .iter()
                .position(|&eh| eh == th)
            {
                let eff_hp = enemy_eff_hp[t_slot];
                let is_lethal = est_dmg > 0 && eff_hp > 0 && est_dmg >= eff_hp;
                if is_lethal {
                    card_has_lethal[h_idx] = true;
                }
                let is_summoner = enemy_is_summoner[t_slot];
                let incoming = enemy_incoming[t_slot].clamp(0, 10_000) as i64;
                let inv_eff_hp = (10_000 - eff_hp).clamp(0, 10_000) as i64;
                let inv_slot = (63usize.saturating_sub(t_slot).min(63)) as i64;
                target_keys[a] = (i64::from(is_lethal) << 44)
                    | (i64::from(is_summoner) << 43)
                    | (incoming << 24)
                    | (inv_eff_hp << 8)
                    | inv_slot;
            }
        } else if est_dmg > 0
            && min_alive_enemy_eff_hp < i32::MAX
            && est_dmg >= min_alive_enemy_eff_hp
        {
            card_has_lethal[h_idx] = true;
            target_keys[a] = 1i64 << 44;
        }
    }

    for h_idx in 0..hand_cards.len() {
        if card_has_lethal[h_idx] {
            card_priority[h_idx] += 1000;
        }
    }

    // Compute per-hand-card smart target rank (0 = best target for that card, 1 = 2nd best, ...).
    let mut action_rank: SmallVec<[usize; 128]> = smallvec::smallvec![0; cand_len];
    let mut max_rank = 0usize;
    for a in 0..cand_len {
        let h_a = play_candidates[a].1;
        let key_a = target_keys[a];
        let mut rank = 0usize;
        for b in 0..cand_len {
            if b == a || play_candidates[b].1 != h_a {
                continue;
            }
            let key_b = target_keys[b];
            if key_b > key_a || (key_b == key_a && b < a) {
                rank += 1;
            }
        }
        action_rank[a] = rank;
        if rank > max_rank {
            max_rank = rank;
        }
    }

    keep_mask[..cand_len].fill(false);
    let mut rank_order: SmallVec<[usize; 128]> = SmallVec::new();
    let mut remaining = max_branches;
    for target_rank in 0..=max_rank {
        if remaining == 0 {
            break;
        }
        rank_order.clear();
        for a in 0..cand_len {
            if action_rank[a] == target_rank {
                rank_order.push(a);
            }
        }
        if rank_order.len() <= remaining {
            for &idx in &rank_order {
                keep_mask[idx] = true;
            }
            remaining -= rank_order.len();
            continue;
        }
        rank_order.sort_unstable_by(|&ia, &ib| {
            let pa = card_priority[play_candidates[ia].1];
            let pb = card_priority[play_candidates[ib].1];
            pb.cmp(&pa)
                .then_with(|| target_keys[ib].cmp(&target_keys[ia]))
                .then_with(|| ia.cmp(&ib))
        });
        for &idx in rank_order.iter().take(remaining) {
            keep_mask[idx] = true;
        }
        remaining = 0;
    }
}

#[allow(clippy::too_many_arguments)]
fn enumerate_parent_tasks(
    parent_rank: u32,
    parent_id: NodeId,
    node: &SearchNode,
    parent_has_choice_lookahead: bool,
    parent_choice_lookahead_score: Option<f64>,
    state: &mut BattleState,
    content: &dyn ContentDispatcher,
    policy: &SearchPolicy,
    profile: &SolverSearchProfile,
    _root_sensitivity: &CombatOrderSensitivity,
    root_baseline: &RootEvalBaseline,
    allow_paid_optional_potions: bool,
    tasks_out: &mut Vec<ActionTask>,
    por_pruned: &mut u64,
    branch_truncations: &mut u64,
) {
    if !state.is_in_progress || state.won || state.pending.is_some() {
        return;
    }

    let sensitivity = CombatOrderSensitivity::analyze_live(state, content);

    let parent_filled_potions = state
        .player
        .potion_slots
        .iter()
        .filter(|s| s.is_some())
        .count()
        .min(u8::MAX as usize) as u8;

    let prev_por = node.por_class();
    let prev_turn = node.action_turn();
    let curr_turn = state.player.turn_number;

    let mut action_ord = 0u32;

    let make_task = |action_ord: u32,
                     spec: ActionSpec,
                     has_la: bool,
                     la_score: Option<f64>| ActionTask {
        parent_rank,
        parent_id,
        parent_turn: node.turn as i32,
        parent_action_count: node.action_count,
        parent_potion_count: node.potion_count as u16,
        parent_explicit_potion_count: node.explicit_potion_count as u16,
        parent_optional_paid_potion_count: node.optional_paid_potion_count as u16,
        parent_automatic_potion_count: node.automatic_potion_count as u16,
        parent_filled_potions,
        parent_used_potion_slot_mask: node.used_potion_slot_mask,
        parent_potion_cost: node.potion_strategic_cost as i32,
        parent_optional_paid_potion_cost: node.optional_paid_potion_cost as i32,
        parent_ambergris_count: node.ambergris_count as u16,
        parent_forced_used_mask: node.forced_used_mask,
        parent_forced_ambergris_count: node.forced_ambergris_count as u16,
        parent_has_choice_lookahead: has_la,
        parent_choice_lookahead_score: la_score,
        action_ord,
        spec,
    };

    if state.player.phase == PlayerTurnPhase::Play {
        let hand_cards = dedup_playable_hand_cards(
            state,
            content,
            policy.accelerations.enable_hand_potion_dedup,
        );
        let max_branches = if (32..64).contains(&profile.max_card_branches_per_node) {
            64
        } else {
            profile.max_card_branches_per_node
        };
        let mut play_candidates: SmallVec<
            [(usize, usize, CardHandle, Option<CreatureHandle>, PorActionClass); 64],
        > = SmallVec::new();

        for (h_pos, &ch) in hand_cards.iter().enumerate() {
            let Some((card_id, meta)) = state
                .card(ch)
                .and_then(|c| content.card_meta(c.id).ok().map(|m| (c.id, m)))
            else {
                continue;
            };
            let eff_target = state
                .fast_card_resolved_target_type(content, ch, card_id, meta)
                .unwrap_or(meta.target_type);
            let targets = target_candidates_for(state, eff_target);
            let mut rank_for_card = 0usize;
            for target_handle in targets {
                let por_class = classify_card_for_por(state, content, ch, target_handle);
                if policy.accelerations.enable_por
                    && por_should_skip(
                        sensitivity.por_safe,
                        prev_turn,
                        curr_turn,
                        prev_por,
                        por_class,
                    )
                {
                    *por_pruned += 1;
                    continue;
                }
                play_candidates.push((rank_for_card, h_pos, ch, target_handle, por_class));
                rank_for_card += 1;
            }
        }

        if play_candidates.len() <= max_branches {
            for &(_, _, card_handle, target_handle, por_class) in &play_candidates {
                tasks_out.push(make_task(
                    action_ord,
                    ActionSpec::PlayCard {
                        card_handle,
                        target_handle,
                        por_class,
                    },
                    parent_has_choice_lookahead,
                    parent_choice_lookahead_score,
                ));
                action_ord += 1;
            }
        } else {
            *branch_truncations += 1;
            let mut keep_mask: SmallVec<[bool; 128]> =
                smallvec::smallvec![false; play_candidates.len()];
            filter_play_candidates_priority(
                state,
                content,
                &hand_cards,
                &play_candidates,
                max_branches,
                &mut keep_mask,
            );
            for (i, &(_, _, card_handle, target_handle, por_class)) in
                play_candidates.iter().enumerate()
            {
                if !keep_mask[i] {
                    continue;
                }
                tasks_out.push(make_task(
                    action_ord,
                    ActionSpec::PlayCard {
                        card_handle,
                        target_handle,
                        por_class,
                    },
                    parent_has_choice_lookahead,
                    parent_choice_lookahead_score,
                ));
                action_ord += 1;
            }
        }

        if policy.allows_any_potion() {
            let potions = dedup_and_filter_potions_with_root(
                state,
                &sensitivity,
                policy,
                Some(root_baseline),
                policy.accelerations.enable_hand_potion_dedup,
                allow_paid_optional_potions,
                root_baseline.renewable_potion_shaped_rock,
            );
            for (slot, pid, cls) in potions {
                let Ok(meta) = content.potion_meta(pid) else {
                    continue;
                };
                if !policy.allow_random_cards && meta.uses_rng {
                    continue;
                }
                let targets = target_candidates_for(state, meta.target_type);
                for target_handle in targets {
                    tasks_out.push(make_task(
                        action_ord,
                        ActionSpec::UsePotion {
                            potion_slot: slot,
                            potion_id: pid,
                            target_handle,
                            is_generated: cls.is_generated,
                            is_forced: cls.is_forced,
                            optional_paid_cost_delta: cls.optional_paid_cost_delta,
                            optional_paid_count_delta: cls.optional_paid_count_delta,
                            root_potion_slot_bit: cls.root_potion_slot_bit,
                            forced_bit: cls.forced_bit,
                        },
                        parent_has_choice_lookahead,
                        parent_choice_lookahead_score,
                    ));
                    action_ord += 1;
                }
            }
        }
    }

    // EndTurn action
    tasks_out.push(make_task(action_ord, ActionSpec::EndTurn, false, None));
}

struct StagedCandidateFields {
    has_extra_end_turn: bool,
    state_slab_idx: u32,
    state_off: u32,
    state_len: u32,
    state_hash: StateHash,
    turn: i32,
    turn_completed: bool,
    action_count: u16,
    potion_count: u16,
    explicit_potion_count: u16,
    optional_paid_potion_count: u16,
    automatic_potion_count: u16,
    used_potion_slot_mask: u16,
    potion_strategic_cost: i32,
    optional_paid_potion_cost: i32,
    ambergris_count: u16,
    forced_used_mask: u64,
    forced_ambergris_count: u16,
    is_terminal: bool,
    fast_eval: FastEvalSummary,
    full_eval: Option<FullEvalDetails>,
    label: TranspositionLabel,
}

#[allow(clippy::too_many_arguments)]
fn prepare_staged_candidate(
    scratch: &mut WorkerScratch,
    task: &ActionTask,
    has_extra_end_turn: bool,
    check_local_choice_tt: bool,
    inherit_lookahead: bool,
    use_choice_slab: bool,
    defer_pack: bool,
    content: &dyn ContentDispatcher,
    root_baseline: &RootEvalBaseline,
    policy: &SearchPolicy,
    profile: &SolverSearchProfile,
    cert: &ClosureCertificate,
    frozen_incumbent: &FrozenIncumbentBound,
    tt: &ShardedParetoTT,
    sensitivity: &CombatOrderSensitivity,
    allow_paid_optional_potions: bool,
    eval_mode: EvalMode,
) -> Option<StagedCandidateFields> {
    if policy.brightest_flame_max_hp_loss_limit.is_some()
        && !allows_brightest_flame(
            policy.brightest_flame_max_hp_loss_limit,
            root_baseline.initial_brightest_flame_max_hp_spent,
            root_baseline.brightest_flame_spent(&scratch.state),
        )
    {
        return None;
    }
    let mut final_extra_end = has_extra_end_turn;
    if !final_extra_end
        && policy.accelerations.enable_forced_end_turn_compression
        && !matches!(task.spec, ActionSpec::EndTurn | ActionSpec::TurnSetup)
        && scratch.state.is_in_progress
        && !scratch.state.won
        && scratch.state.pending.is_none()
        && scratch.state.player.phase == PlayerTurnPhase::Play
    {
        let has_card = has_any_playable_hand_card(&mut scratch.state, content);
        let has_potion = !has_card
            && policy.allows_any_potion()
            && {
                let live_sens = CombatOrderSensitivity {
                    any_heal_hook: CombatOrderSensitivity::has_live_heal_hook(
                        &mut scratch.state,
                        content,
                    ),
                    ..*sensitivity
                };
                has_any_usable_potion_with_root(
                    &scratch.state,
                    &live_sens,
                    policy,
                    Some(root_baseline),
                    allow_paid_optional_potions,
                    root_baseline.renewable_potion_shaped_rock,
                )
            };
        if !has_card && !has_potion {
            clone_battle_state_into(&scratch.state, &mut scratch.pre_end_turn_state);
            match CombatEngine::end_player_turn(&mut scratch.state, content) {
                Ok(StepOutcome::Completed) | Ok(StepOutcome::CombatEnded { .. }) => {
                    if policy.brightest_flame_max_hp_loss_limit.is_some()
                        && !allows_brightest_flame(
                            policy.brightest_flame_max_hp_loss_limit,
                            root_baseline.initial_brightest_flame_max_hp_spent,
                            root_baseline.brightest_flame_spent(&scratch.state),
                        )
                    {
                        return None;
                    }
                    final_extra_end = true;
                }
                Ok(StepOutcome::NeedsChoice(_)) => {
                    std::mem::swap(&mut scratch.state, &mut scratch.pre_end_turn_state);
                }
                Err(err) => {
                    if should_abort_search_on_kernel_error(&err) {
                        scratch.record_fatal_error(err);
                        return None;
                    }
                    std::mem::swap(&mut scratch.state, &mut scratch.pre_end_turn_state);
                }
            }
        }
    }

    let fast_eval = evaluate_fast(&scratch.state, content, root_baseline, None);

    let (
        explicit_delta,
        optional_paid_count_delta,
        optional_paid_cost_delta,
        slot_cost_raw,
        ambergris_delta,
        slot_mask_bit,
        forced_mask_bit,
        forced_amb_delta,
    ) = match task.spec {
        ActionSpec::UsePotion {
            potion_id,
            is_generated,
            is_forced,
            optional_paid_cost_delta,
            optional_paid_count_delta,
            root_potion_slot_bit,
            forced_bit,
            ..
        } => {
            if is_generated {
                (1u16, 0u16, 0i32, 0i32, 0u16, 0u16, 0u64, 0u16)
            } else {
                let cost = if is_forced {
                    PotionValuationRegistry::strategic_hp_cost_for_root(
                        potion_id,
                        root_baseline.renewable_potion_shaped_rock,
                        root_baseline.initial_player_max_hp,
                        &policy.potion_hp_threshold_overrides,
                    )
                } else {
                    optional_paid_cost_delta
                };
                let amb = u16::from(potion_id == PotionId::Ambergris);
                let f_bit = forced_bit.filter(|&b| b < 64).map_or(0u64, |b| 1u64 << b);
                let newly_forced = f_bit != 0 && (task.parent_forced_used_mask & f_bit) == 0;
                (
                    1u16,
                    optional_paid_count_delta,
                    optional_paid_cost_delta,
                    cost,
                    amb,
                    root_potion_slot_bit,
                    f_bit,
                    if newly_forced { amb } else { 0 },
                )
            }
        }
        _ => (0u16, 0u16, 0i32, 0i32, 0u16, 0u16, 0u64, 0u16),
    };

    let after_filled = scratch
        .state
        .player
        .potion_slots
        .iter()
        .filter(|s| s.is_some())
        .count() as i32;
    let total_consumed = (i32::from(task.parent_filled_potions) - after_filled).max(0);
    let auto_delta = (total_consumed - i32::from(explicit_delta)).max(0) as u16;

    let potion_count = task.parent_potion_count + explicit_delta + auto_delta;
    let explicit_potion_count = task.parent_explicit_potion_count + explicit_delta;
    let optional_paid_potion_count =
        task.parent_optional_paid_potion_count + optional_paid_count_delta;
    let automatic_potion_count = task.parent_automatic_potion_count + auto_delta;
    let used_potion_slot_mask = task.parent_used_potion_slot_mask | slot_mask_bit;
    let potion_strategic_cost = task.parent_potion_cost + slot_cost_raw;
    let optional_paid_potion_cost =
        task.parent_optional_paid_potion_cost + optional_paid_cost_delta;
    let ambergris_count = task.parent_ambergris_count + ambergris_delta;
    let forced_used_mask = task.parent_forced_used_mask | forced_mask_bit;
    let forced_ambergris_count = task.parent_forced_ambergris_count + forced_amb_delta;
    let action_count = task.parent_action_count + 1 + u16::from(final_extra_end);
    let turn = scratch.state.player.turn_number;
    let turn_completed = matches!(task.spec, ActionSpec::EndTurn)
        || final_extra_end
        || turn > task.parent_turn;
    let is_terminal = !scratch.state.is_in_progress
        || fast_eval.won
        || fast_eval.player_dead
        || fast_eval.is_event_defeat
        || turn - root_baseline.start_turn_number >= policy.max_turns;

    let net_potion_cost_centi = PotionUsePolicy::compute_net_potion_opportunity_cost_centi(
        optional_paid_potion_cost,
        optional_paid_potion_count as i32,
        fast_eval.alchemize_count as i32,
        fast_eval.empty_potion_slots as i32,
        root_baseline,
    );

    if !is_terminal
        && policy.accelerations.enable_closure_pruning
        && frozen_incumbent.is_pruned(
            cert,
            root_baseline,
            fast_eval.player_hp,
            fast_eval.player_max_hp,
            fast_eval.death_save_hp_restored,
            fast_eval.death_save_use_count,
            fast_eval.feed_count,
            fast_eval.won,
            fast_eval.player_dead,
            turn,
            fast_eval.cumulative_hp_lost,
            potion_count as usize,
            optional_paid_potion_count as usize,
            explicit_potion_count as usize,
            net_potion_cost_centi,
            policy.theft_policy == SolverTheftPolicy::PreserveResources,
        )
    {
        scratch.incumbent_pruned += 1;
        return None;
    }

    let state_hash = hash_canonical_key(&scratch.state);
    if check_local_choice_tt && !scratch.try_insert_local_choice_tt(state_hash) {
        return None;
    }
    if !is_terminal {
        tt.prefetch(state_hash);
    }
    let full_eval = if !is_terminal {
        let mut full = evaluate_full_details_cached(
            &scratch.state,
            content,
            &fast_eval,
            root_baseline,
            Some(&mut scratch.eval_intent_cache),
        );
        full.beam_rank_score = compute_beam_rank_score_with_centi(
            &fast_eval,
            &full,
            root_baseline,
            profile.base_score_only,
            profile.boss_tempo_hp_pricing,
            profile.beam_weight_perturbation,
            eval_mode,
            net_potion_cost_centi,
            action_count,
        );
        if inherit_lookahead && !turn_completed && task.parent_has_choice_lookahead {
            full.has_choice_lookahead = true;
            full.choice_lookahead_score = task.parent_choice_lookahead_score;
        }
        Some(full)
    } else {
        None
    };

    let tt_score = full_eval
        .as_ref()
        .map_or(fast_eval.base_score, |f| f.beam_rank_score);
    let label = TranspositionLabel::new(
        forced_used_mask,
        fast_eval.death_save_use_count,
        potion_count,
        optional_paid_potion_cost,
        0,
        fast_eval.cumulative_hp_lost,
        action_count,
        tt_score,
    )
    .with_details(
        explicit_potion_count > 0,
        optional_paid_potion_count.min(u8::MAX as u16) as u8,
        fast_eval.alchemize_count,
        fast_eval
            .death_save_hp_restored
            .clamp(i16::MIN as i32, i16::MAX as i32) as i16,
    );

    if !is_terminal && tt.probe_dominated(state_hash, &label) {
        scratch.tt_probe_pruned += 1;
        return None;
    }

    let (state_slab_idx, state_off, state_len) = if is_terminal || defer_pack {
        (0, 0, 0)
    } else {
        pack_state(&scratch.state, &mut scratch.pack_buf);
        if use_choice_slab {
            scratch.choice_slab.alloc_slice(&scratch.pack_buf)
        } else {
            scratch.slab.alloc_slice(&scratch.pack_buf)
        }
    };

    Some(StagedCandidateFields {
        has_extra_end_turn: final_extra_end,
        state_slab_idx,
        state_off,
        state_len,
        state_hash,
        turn,
        turn_completed,
        action_count,
        potion_count,
        explicit_potion_count,
        optional_paid_potion_count,
        automatic_potion_count,
        used_potion_slot_mask,
        potion_strategic_cost,
        optional_paid_potion_cost,
        ambergris_count,
        forced_used_mask,
        forced_ambergris_count,
        is_terminal,
        fast_eval,
        full_eval,
        label,
    })
}

#[inline]
fn rare_eval_opt(f: &FastEvalSummary) -> Option<RareNodeEval> {
    if f.growth_rewards.is_zero() && f.relic_eval.target_mask == 0 {
        None
    } else {
        Some(RareNodeEval {
            growth_rewards: f.growth_rewards,
            relic_eval: f.relic_eval,
        })
    }
}

#[inline]
fn choice_probe_limits(
    profile: &SolverSearchProfile,
    first_pile: PileType,
    is_end_turn: bool,
    is_turn_setup: bool,
) -> (bool, usize, usize) {
    let max_branches = if is_turn_setup {
        profile
            .beam_width
            .max(64)
            .clamp(64, TURN_SETUP_CHOICE_RESOLVED_CAPACITY)
    } else if first_pile == PileType::Hand {
        profile.max_hand_choice_branches_per_action.clamp(1, 64)
    } else {
        profile.max_pile_choice_branches_per_action.clamp(1, 64)
    };
    let resolved_cap = if is_turn_setup {
        max_branches
            .max(CHOICE_RESOLVED_CAPACITY)
            .clamp(CHOICE_RESOLVED_CAPACITY, TURN_SETUP_CHOICE_RESOLVED_CAPACITY)
    } else {
        CHOICE_RESOLVED_CAPACITY
    };
    let is_delayed_or_end_turn = is_end_turn
        || is_turn_setup
        || matches!(
            first_pile,
            PileType::Draw | PileType::Discard | PileType::Exhaust
        );
    (is_delayed_or_end_turn, max_branches, resolved_cap)
}

#[inline]
fn encode_choice_probe_rank_score_with_policy(
    probe: &ChoiceProbeResult,
    root_baseline: &RootEvalBaseline,
) -> f64 {
    if probe.is_victory {
        let s = probe
            .score
            .clamp(-1_000_000_000_000_000.0, 1_000_000_000_000_000.0);
        let mut rank = 2_000_000_000_000_000.0 + s - (probe.hp_lost as f64) * 100_000.0;
        let has_strategy_targets = root_baseline.effective_has_growth_targets
            || !root_baseline.active_relic_targets.is_empty();
        if has_strategy_targets {
            rank += (probe.strategy_goal_hp_credit as f64) * 100_000.0;
        }
        return rank;
    }
    if !probe.is_defeat && probe.is_event_defeat {
        let s = probe
            .score
            .clamp(-1_000_000_000_000_000.0, 1_000_000_000_000_000.0);
        return -1_500_000_000_000.0 + s;
    }
    encode_choice_probe_rank_score(probe)
}

#[inline]
fn hash_step0_choices(step0: Option<&SmallVec<[usize; 8]>>) -> u64 {
    let Some(sel) = step0 else {
        return 0;
    };
    let mut h = 0x517C_C1B7_2722_0A95u64 ^ (sel.len() as u64);
    for &idx in sel {
        h = h
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(idx as u64 + 1);
        h ^= h >> 29;
    }
    h
}

#[inline]
fn count_choice_combinations_capped(n: usize, k: usize, cap: usize) -> usize {
    if k == 0 {
        return 1;
    }
    if k > n {
        return 0;
    }
    let kk = k.min(n - k);
    let mut comb = 1usize;
    for i in 1..=kk {
        comb = comb.saturating_mul(n - kk + i) / i;
        if comb >= cap {
            return cap;
        }
    }
    comb
}

#[inline]
fn total_choice_combos_exceeds(
    n: usize,
    min_count: usize,
    max_count: usize,
    can_skip: bool,
    resolved_cap: usize,
) -> bool {
    if n == 0 || max_count == 0 {
        return false;
    }
    let is_optional = min_count == 0 || can_skip;
    if is_optional && max_count >= n {
        return n >= 16 || (1usize << n) > resolved_cap;
    }
    let mut total = usize::from(can_skip && min_count == 0);
    let min_pick = min_count.max(usize::from(can_skip));
    let max_pick = max_count.min(n);
    if min_pick > max_pick {
        return false;
    }
    for k in min_pick..=max_pick {
        total = total.saturating_add(count_choice_combinations_capped(
            n,
            k,
            resolved_cap + 1,
        ));
        if total > resolved_cap {
            return true;
        }
    }
    false
}

#[allow(clippy::too_many_arguments)]
fn finalize_candidate(
    scratch: &mut WorkerScratch,
    task: &ActionTask,
    action: CompactAction,
    por_key: u64,
    has_extra_end_turn: bool,
    choices: &[SmallVec<[usize; 8]>],
    choice_ord: u32,
    content: &dyn ContentDispatcher,
    root_baseline: &RootEvalBaseline,
    policy: &SearchPolicy,
    profile: &SolverSearchProfile,
    cert: &ClosureCertificate,
    frozen_incumbent: &FrozenIncumbentBound,
    tt: &ShardedParetoTT,
    sensitivity: &CombatOrderSensitivity,
    allow_paid_optional_potions: bool,
    eval_mode: EvalMode,
) {
    let Some(s) = prepare_staged_candidate(
        scratch,
        task,
        has_extra_end_turn,
        false,
        choices.is_empty(),
        false,
        false,
        content,
        root_baseline,
        policy,
        profile,
        cert,
        frozen_incumbent,
        tt,
        sensitivity,
        allow_paid_optional_potions,
        eval_mode,
    ) else {
        return;
    };

    let (choices_offset, choices_len) = scratch.staged_choices.push_choices(choices);
    let base_score = s.fast_eval.base_score;
    let rare_eval = rare_eval_opt(&s.fast_eval);
    let stored_fast = StoredFastEval::from(&s.fast_eval);
    let stored_full = s.full_eval.map(|f| f.to_stored());
    let fast_beam_score = stored_full
        .as_ref()
        .map_or(base_score, |f| f.beam_rank_score);
    scratch.cand_shards.push(if s.is_terminal {
        u16::MAX
    } else {
        s.state_hash.shard_index() as u16
    });
    scratch.cand_fast_meta.push(WorkerCandFastMeta {
        flags: u8::from(s.is_terminal) | (u8::from(s.turn_completed) << 1),
        potion_count: s.potion_count.min(u8::MAX as u16) as u8,
        explicit_potion_count: s.explicit_potion_count.min(u8::MAX as u16) as u8,
        _pad: [0; 5],
        beam_score: fast_beam_score,
    });
    scratch.candidates.push(WorkerCandidate {
        sort_key: (task.parent_rank, task.action_ord, choice_ord),
        parent_id: task.parent_id,
        action,
        por_key,
        has_extra_end_turn: s.has_extra_end_turn,
        choices_offset,
        choices_len,
        state_slab_idx: s.state_slab_idx,
        state_off: s.state_off,
        state_len: s.state_len,
        state_hash: s.state_hash,
        turn: s.turn,
        turn_completed: s.turn_completed,
        action_count: s.action_count,
        potion_count: s.potion_count,
        explicit_potion_count: s.explicit_potion_count,
        optional_paid_potion_count: s.optional_paid_potion_count,
        automatic_potion_count: s.automatic_potion_count,
        used_potion_slot_mask: s.used_potion_slot_mask,
        potion_strategic_cost: s.potion_strategic_cost,
        optional_paid_potion_cost: s.optional_paid_potion_cost,
        ambergris_count: s.ambergris_count,
        forced_used_mask: s.forced_used_mask,
        forced_ambergris_count: s.forced_ambergris_count,
        is_terminal: s.is_terminal,
        base_score,
        fast_eval: stored_fast,
        full_eval: stored_full,
        rare_eval,
        label: s.label,
    });
}

fn replay_deferred_choice_state_from_arena(
    paused_base: &BattleState,
    dst: &mut BattleState,
    choice_arena: &ChoiceArena,
    choices_offset: u32,
    choices_len: u8,
    has_extra_end_turn: bool,
    content: &dyn ContentDispatcher,
) -> bool {
    clone_battle_state_into(paused_base, dst);
    let mut sel_buf: SmallVec<[usize; 16]> = SmallVec::new();
    for step_idx in 0..(choices_len as usize) {
        choice_arena.copy_step_indices(choices_offset, step_idx, &mut sel_buf);
        if CombatEngine::resume_choice(dst, content, &sel_buf).is_err() {
            return false;
        }
    }
    if has_extra_end_turn {
        match CombatEngine::end_player_turn(dst, content) {
            Ok(StepOutcome::Completed) | Ok(StepOutcome::CombatEnded { .. }) => {}
            _ => return false,
        }
    }
    true
}

#[allow(clippy::too_many_arguments)]
fn stage_resolved_choice(
    scratch: &mut WorkerScratch,
    task: &ActionTask,
    action: CompactAction,
    por_key: u64,
    choices: &[SmallVec<[usize; 8]>],
    defer_pack: bool,
    content: &dyn ContentDispatcher,
    root_baseline: &RootEvalBaseline,
    policy: &SearchPolicy,
    profile: &SolverSearchProfile,
    cert: &ClosureCertificate,
    frozen_incumbent: &FrozenIncumbentBound,
    tt: &ShardedParetoTT,
    sensitivity: &CombatOrderSensitivity,
    allow_paid_optional_potions: bool,
    eval_mode: EvalMode,
) {
    let Some(s) = prepare_staged_candidate(
        scratch,
        task,
        false,
        true,
        false,
        true,
        defer_pack,
        content,
        root_baseline,
        policy,
        profile,
        cert,
        frozen_incumbent,
        tt,
        sensitivity,
        allow_paid_optional_potions,
        eval_mode,
    ) else {
        return;
    };

    let (choices_offset, choices_len) = scratch.choice_staging_choices.push_choices(choices);
    let depth0_sig = hash_step0_choices(choices.first());

    let terminal_probe_score = if s.is_terminal {
        let net_potion_cost_centi =
            PotionUsePolicy::compute_net_potion_opportunity_cost_centi(
                s.optional_paid_potion_cost,
                s.optional_paid_potion_count as i32,
                s.fast_eval.alchemize_count as i32,
                s.fast_eval.empty_potion_slots as i32,
                root_baseline,
            );
        compute_beam_rank_score_with_centi(
            &s.fast_eval,
            &FullEvalDetails::default(),
            root_baseline,
            profile.base_score_only,
            profile.boss_tempo_hp_pricing,
            profile.beam_weight_perturbation,
            eval_mode,
            net_potion_cost_centi,
            s.action_count,
        )
    } else {
        0.0
    };
    let base_score = s.fast_eval.base_score;
    let rare_eval = rare_eval_opt(&s.fast_eval);
    let stored_fast = StoredFastEval::from(&s.fast_eval);
    let stored_full = s.full_eval.map(|f| f.to_stored());
    let lookahead_board_investment = stored_full.as_ref().map_or(0, |f| f.board_investment);
    let lookahead_setup_score = stored_full.as_ref().map_or(0, |f| f.setup_score);
    scratch.choice_resolved.push(ResolvedChoiceCandidate {
        choices_offset,
        choices_len,
        depth0_sig,
        action,
        por_key,
        has_extra_end_turn: s.has_extra_end_turn,
        state_slab_idx: s.state_slab_idx,
        state_off: s.state_off,
        state_len: s.state_len,
        state_hash: s.state_hash,
        turn: s.turn,
        turn_completed: s.turn_completed,
        action_count: s.action_count,
        potion_count: s.potion_count,
        explicit_potion_count: s.explicit_potion_count,
        optional_paid_potion_count: s.optional_paid_potion_count,
        automatic_potion_count: s.automatic_potion_count,
        used_potion_slot_mask: s.used_potion_slot_mask,
        potion_strategic_cost: s.potion_strategic_cost,
        optional_paid_potion_cost: s.optional_paid_potion_cost,
        ambergris_count: s.ambergris_count,
        forced_used_mask: s.forced_used_mask,
        forced_ambergris_count: s.forced_ambergris_count,
        is_terminal: s.is_terminal,
        base_score,
        fast_eval: stored_fast,
        full_eval: stored_full,
        rare_eval,
        label: s.label,
        terminal_probe_score,
        lookahead_score: 0.0,
        lookahead_hp_lost: s.fast_eval.cumulative_hp_lost,
        lookahead_board_investment,
        lookahead_setup_score,
        has_lookahead: false,
        lookahead_victory: s.fast_eval.won,
    });
}

#[allow(clippy::too_many_arguments)]
fn probe_choice_selection_1t(
    scratch: &mut WorkerScratch,
    _static_ctx: &Arc<RootStaticContext>,
    content: &dyn ContentDispatcher,
    root_baseline: &RootEvalBaseline,
    profile: &SolverSearchProfile,
    eval_mode: EvalMode,
    probe_cache: &mut ProbeCacheAccess<'_>,
    task: &ActionTask,
    sel: &[usize],
) -> f64 {
    clone_battle_state_into(&scratch.choice_paused_states[0], &mut scratch.state);
    scratch.choice_replay_attempts += 1;
    scratch.transitions += 1;
    let mut out = match CombatEngine::resume_choice(&mut scratch.state, content, sel) {
        Ok(out) => out,
        Err(err) => {
            if should_abort_search_on_kernel_error(&err) {
                scratch.record_fatal_error(err);
            }
            return f64::NEG_INFINITY;
        }
    };
    let mut step_guard = 0usize;
    while let StepOutcome::NeedsChoice(sub_req) = out {
        step_guard += 1;
        if step_guard >= 4 || sub_req.candidates.is_empty() {
            if sub_req.min_count == 0 {
                scratch.choice_replay_attempts += 1;
                match CombatEngine::resume_choice(&mut scratch.state, content, &[]) {
                    Ok(next) => {
                        out = next;
                        continue;
                    }
                    Err(err) => {
                        if should_abort_search_on_kernel_error(&err) {
                            scratch.record_fatal_error(err);
                        }
                    }
                }
            }
            return f64::NEG_INFINITY;
        }
        let fallback_sel: SmallVec<[usize; 8]> = if sub_req.min_count == 0 {
            SmallVec::new()
        } else {
            let take = sub_req.min_count.min(sub_req.candidates.len());
            let start = sub_req.candidates.len() - take;
            (start..sub_req.candidates.len()).collect()
        };
        scratch.choice_replay_attempts += 1;
        let next = match CombatEngine::resume_choice(&mut scratch.state, content, &fallback_sel) {
            Ok(next) => next,
            Err(err) => {
                if should_abort_search_on_kernel_error(&err) {
                    scratch.record_fatal_error(err);
                }
                return f64::NEG_INFINITY;
            }
        };
        out = next;
    }

    let (opt_cost, opt_count) = match task.spec {
        ActionSpec::UsePotion {
            optional_paid_cost_delta,
            optional_paid_count_delta,
            ..
        } => (
            task.parent_optional_paid_potion_cost + optional_paid_cost_delta,
            task.parent_optional_paid_potion_count + optional_paid_count_delta,
        ),
        _ => (
            task.parent_optional_paid_potion_cost,
            task.parent_optional_paid_potion_count,
        ),
    };
    let state_hash = hash_canonical_key(&scratch.state);
    let paused_base = &scratch.choice_paused_states[0];
    let mut restore_fatal: Option<KernelError> = None;
    let probe_opt = evaluate_choice_probe_in_place_with_restore(
        &mut scratch.choice_probe_ws,
        &mut scratch.state,
        |s| {
            clone_battle_state_into(paused_base, s);
            let mut re_out = match CombatEngine::resume_choice(s, content, sel) {
                Ok(out) => out,
                Err(err) => {
                    if should_abort_search_on_kernel_error(&err) {
                        restore_fatal = Some(err);
                    }
                    return false;
                }
            };
            let mut re_guard = 0usize;
            while let StepOutcome::NeedsChoice(sub_req) = re_out {
                re_guard += 1;
                if re_guard >= 4 || sub_req.candidates.is_empty() {
                    if sub_req.min_count == 0 {
                        match CombatEngine::resume_choice(s, content, &[]) {
                            Ok(next) => {
                                re_out = next;
                                continue;
                            }
                            Err(err) => {
                                if should_abort_search_on_kernel_error(&err) {
                                    restore_fatal = Some(err);
                                }
                            }
                        }
                    }
                    return false;
                }
                let fallback_sel: SmallVec<[usize; 8]> = if sub_req.min_count == 0 {
                    SmallVec::new()
                } else {
                    let take = sub_req.min_count.min(sub_req.candidates.len());
                    let start = sub_req.candidates.len() - take;
                    (start..sub_req.candidates.len()).collect()
                };
                let next = match CombatEngine::resume_choice(s, content, &fallback_sel) {
                    Ok(next) => next,
                    Err(err) => {
                        if should_abort_search_on_kernel_error(&err) {
                            restore_fatal = Some(err);
                        }
                        return false;
                    }
                };
                re_out = next;
            }
            true
        },
        state_hash,
        probe_cache,
        content,
        root_baseline,
        profile,
        eval_mode,
        1,
        opt_cost,
        opt_count as usize,
        task.parent_action_count + 1,
    );
    if let Some(err) = restore_fatal {
        scratch.record_fatal_error(err);
        return f64::NEG_INFINITY;
    }
    let Some(probe) = probe_opt else {
        return f64::NEG_INFINITY;
    };
    encode_choice_probe_rank_score_with_policy(&probe, root_baseline)
}

#[allow(clippy::too_many_arguments)]
fn screen_high_branching_multi_pick(
    scratch: &mut WorkerScratch,
    choices_so_far: &mut SmallVec<[SmallVec<[usize; 8]>; 2]>,
    task: &ActionTask,
    action: CompactAction,
    por_key: u64,
    is_delayed_or_end_turn: bool,
    is_turn_setup: bool,
    max_branches: usize,
    resolved_cap: usize,
    max_choice_depth: usize,
    defer_choice_probes: bool,
    static_ctx: &Arc<RootStaticContext>,
    content: &dyn ContentDispatcher,
    root_baseline: &RootEvalBaseline,
    policy: &SearchPolicy,
    profile: &SolverSearchProfile,
    cert: &ClosureCertificate,
    frozen_incumbent: &FrozenIncumbentBound,
    tt: &ShardedParetoTT,
    probe_cache: &mut ProbeCacheAccess<'_>,
    sensitivity: &CombatOrderSensitivity,
    allow_paid_optional_potions: bool,
    eval_mode: EvalMode,
    req: &sts2_kernel::continuation::ChoiceRequest,
) -> bool {
    let n = req.candidates.len();
    let mut groups: SmallVec<[(usize, SmallVec<[usize; 4]>); 24]> = SmallVec::new();
    if policy.accelerations.enable_multiset_choice_dedup {
        let mut keys: SmallVec<[HandCardEquivKey; 24]> = SmallVec::with_capacity(n);
        for i in 0..n {
            let k = card_equiv_key(
                &mut scratch.choice_paused_states[0],
                content,
                req.candidates[i],
            );
            if let Some(pos) = keys.iter().position(|&ek| ek == k) {
                groups[pos].1.push(i);
            } else {
                keys.push(k);
                groups.push((i, smallvec::smallvec![i]));
            }
        }
    } else {
        for i in 0..n {
            groups.push((i, smallvec::smallvec![i]));
        }
    }

    let distinct_n = groups.len();
    if !total_choice_combos_exceeds(
        distinct_n,
        req.min_count,
        req.max_count,
        req.can_skip,
        resolved_cap,
    ) {
        return false;
    }

    let is_optional_multi = (req.min_count == 0 || req.can_skip || req.min_count == 1)
        && req.max_count > 1
        && distinct_n > 6;
    let is_fixed_k_multi =
        !is_optional_multi && req.min_count > 1 && req.max_count >= req.min_count && distinct_n > 4;
    if !is_optional_multi && !is_fixed_k_multi {
        return false;
    }

    if is_optional_multi {
        let mut scores = std::mem::take(&mut scratch.marginal_scores);
        let mut order = std::mem::take(&mut scratch.marginal_order);
        scores.clear();
        order.clear();
        scores.reserve(distinct_n);
        order.reserve(distinct_n);
        for g in 0..distinct_n {
            if scratch.fatal_error.is_some() {
                break;
            }
            let rep = groups[g].0;
            let sc = probe_choice_selection_1t(
                scratch,
                static_ctx,
                content,
                root_baseline,
                profile,
                eval_mode,
                probe_cache,
                task,
                &[rep],
            );
            scores.push(sc);
            order.push(g);
        }
        if scratch.fatal_error.is_some() {
            scratch.marginal_scores = scores;
            scratch.marginal_order = order;
            return true;
        }
        order.sort_unstable_by(|&a, &b| scores[b].total_cmp(&scores[a]).then_with(|| a.cmp(&b)));

        let mut greedy_flat: SmallVec<[usize; 24]> = SmallVec::new();
        for &g in &order {
            greedy_flat.extend_from_slice(&groups[g].1);
        }
        let exhaustive_keep = if resolved_cap >= 256 { 7 } else { 6 };
        let max_greedy = greedy_flat.len().min(req.max_count).min(15);
        for subset_size in (exhaustive_keep + 1)..=max_greedy {
            if scratch.fatal_error.is_some() || scratch.choice_resolved.len() >= resolved_cap {
                break;
            }
            let mut sel: SmallVec<[usize; 8]> = greedy_flat[..subset_size].iter().copied().collect();
            sel.sort_unstable();
            clone_battle_state_into(&scratch.choice_paused_states[0], &mut scratch.state);
            scratch.choice_replay_attempts += 1;
            scratch.transitions += 1;
            let outcome = match CombatEngine::resume_choice(&mut scratch.state, content, &sel) {
                Ok(outcome) => outcome,
                Err(err) => {
                    if should_abort_search_on_kernel_error(&err) {
                        scratch.record_fatal_error(err);
                        break;
                    }
                    continue;
                }
            };
            choices_so_far.push(sel);
            match outcome {
                StepOutcome::Completed | StepOutcome::CombatEnded { .. } => {
                    stage_resolved_choice(
                        scratch,
                        task,
                        action,
                        por_key,
                        choices_so_far.as_slice(),
                        !is_turn_setup && !defer_choice_probes,
                        content,
                        root_baseline,
                        policy,
                        profile,
                        cert,
                        frozen_incumbent,
                        tt,
                        sensitivity,
                        allow_paid_optional_potions,
                        eval_mode,
                    );
                }
                StepOutcome::NeedsChoice(_) => {
                    if allows_brightest_flame(
                        policy.brightest_flame_max_hp_loss_limit,
                        root_baseline.initial_brightest_flame_max_hp_spent,
                        root_baseline.brightest_flame_spent(&scratch.state),
                    ) {
                        scratch.ensure_choice_depth(1, static_ctx);
                        clone_battle_state_into(
                            &scratch.state,
                            &mut scratch.choice_paused_states[1],
                        );
                        resolve_choices_recursive(
                            scratch,
                            choices_so_far,
                            1,
                            task,
                            action,
                            por_key,
                            is_delayed_or_end_turn,
                            is_turn_setup,
                            max_branches,
                            resolved_cap,
                            max_choice_depth,
                            defer_choice_probes,
                            static_ctx,
                            content,
                            root_baseline,
                            policy,
                            profile,
                            cert,
                            frozen_incumbent,
                            tt,
                            probe_cache,
                            sensitivity,
                            allow_paid_optional_potions,
                            eval_mode,
                        );
                    }
                }
            }
            choices_so_far.pop();
        }

        let keep_groups = exhaustive_keep.min(distinct_n);
        let mut kept_orig: SmallVec<[usize; 24]> = SmallVec::new();
        for &g in &order[..keep_groups] {
            kept_orig.extend_from_slice(&groups[g].1);
        }
        kept_orig.sort_unstable();
        kept_orig.dedup();
        scratch.marginal_scores = scores;
        scratch.marginal_order = order;
        if scratch.fatal_error.is_some() {
            return true;
        }

        let mut sub_req = req.clone();
        sub_req.candidates = kept_orig.iter().map(|&idx| req.candidates[idx]).collect();
        sub_req.max_count = sub_req.max_count.min(kept_orig.len());
        let mut sub_choices = std::mem::take(&mut scratch.choice_enum_bufs[0]);
        let mut enum_scratch = std::mem::take(&mut scratch.choice_enum_scratch);
        enumerate_canonical_choices_with_scratch(
            &mut scratch.choice_paused_states[0],
            content,
            &sub_req,
            resolved_cap,
            policy.accelerations.enable_multiset_choice_dedup,
            &mut enum_scratch,
            &mut sub_choices,
        );
        scratch.choice_enum_scratch = enum_scratch;
        for v in &mut sub_choices {
            for j in v.iter_mut() {
                *j = kept_orig[*j];
            }
            v.sort_unstable();
        }
        scratch.choice_enum_bufs[0] = sub_choices;
        return true;
    }

    // Fixed-k multi-pick
    let k = req.min_count.min(n);
    let mut tail_anchors: SmallVec<[usize; 8]> = SmallVec::new();
    for g in (0..distinct_n).rev().take(k + 1) {
        tail_anchors.push(groups[g].0);
    }
    let mut head_anchors: SmallVec<[usize; 8]> = SmallVec::new();
    for cand_g in [
        0usize,
        distinct_n / 2,
        1usize,
        (distinct_n / 2) + 1,
        2usize,
        (distinct_n / 2) + 2,
    ] {
        if cand_g < distinct_n {
            let rep = groups[cand_g].0;
            if !head_anchors.contains(&rep) {
                head_anchors.push(rep);
            }
        }
    }

    let build_k_selection = |pivot: usize, anchors: &[usize]| -> SmallVec<[usize; 8]> {
        let mut sel: SmallVec<[usize; 8]> = SmallVec::new();
        sel.push(pivot);
        for &a in anchors {
            if sel.len() >= k {
                break;
            }
            if !sel.contains(&a) {
                sel.push(a);
            }
        }
        for idx in 0..n {
            if sel.len() >= k {
                break;
            }
            if !sel.contains(&idx) {
                sel.push(idx);
            }
        }
        sel.sort_unstable();
        sel
    };

    let mut scores = std::mem::take(&mut scratch.marginal_scores);
    let mut order = std::mem::take(&mut scratch.marginal_order);
    let mut probed_sels = std::mem::take(&mut scratch.probed_sels);
    scores.clear();
    order.clear();
    probed_sels.clear();
    scores.reserve(distinct_n);
    order.reserve(distinct_n);

    let mut probe_cached_sel =
        |scratch: &mut WorkerScratch, sel: &SmallVec<[usize; 8]>| -> f64 {
            if let Some(&(_, sc, rep_d, trans_d)) =
                probed_sels.iter().find(|(s, _, _, _)| s == sel)
            {
                scratch.choice_replay_attempts += rep_d;
                scratch.transitions += trans_d;
                return sc;
            }
            let rep_before = scratch.choice_replay_attempts;
            let trans_before = scratch.transitions;
            let sc = probe_choice_selection_1t(
                scratch,
                static_ctx,
                content,
                root_baseline,
                profile,
                eval_mode,
                probe_cache,
                task,
                sel,
            );
            probed_sels.push((
                sel.clone(),
                sc,
                scratch.choice_replay_attempts - rep_before,
                scratch.transitions - trans_before,
            ));
            sc
        };

    // Round 1: tail vs head/mid anchors across distinct equivalence reps only
    for g in 0..distinct_n {
        if scratch.fatal_error.is_some() {
            break;
        }
        let rep = groups[g].0;
        let sel_tail = build_k_selection(rep, &tail_anchors);
        let s_tail = probe_cached_sel(scratch, &sel_tail);
        if scratch.fatal_error.is_some() {
            break;
        }
        let sel_head = build_k_selection(rep, &head_anchors);
        let s_head = probe_cached_sel(scratch, &sel_head);
        scores.push(s_tail.max(s_head));
        order.push(g);
    }
    if scratch.fatal_error.is_some() {
        probed_sels.clear();
        scratch.probed_sels = probed_sels;
        scratch.marginal_scores = scores;
        scratch.marginal_order = order;
        return true;
    }

    // Round 2: coordinate ascent against Round-1 MVP anchors (top 3 + last)
    order.sort_unstable_by(|&a, &b| scores[b].total_cmp(&scores[a]).then_with(|| a.cmp(&b)));
    let mut mvp_anchors: SmallVec<[usize; 8]> = SmallVec::new();
    for &g in order.iter().take(3) {
        mvp_anchors.push(groups[g].0);
    }
    if let Some(&last_g) = order.last() {
        let rep = groups[last_g].0;
        if !mvp_anchors.contains(&rep) {
            mvp_anchors.push(rep);
        }
    }
    for g in 0..distinct_n {
        if scratch.fatal_error.is_some() {
            break;
        }
        let sel_mvp = build_k_selection(groups[g].0, &mvp_anchors);
        let s_mvp = probe_cached_sel(scratch, &sel_mvp);
        if s_mvp > scores[g] {
            scores[g] = s_mvp;
        }
    }
    if scratch.fatal_error.is_some() {
        probed_sels.clear();
        scratch.probed_sels = probed_sels;
        scratch.marginal_scores = scores;
        scratch.marginal_order = order;
        return true;
    }
    order.sort_unstable_by(|&a, &b| scores[b].total_cmp(&scores[a]).then_with(|| a.cmp(&b)));

    let mut target_s = if k <= 2 {
        12
    } else if k == 3 {
        10
    } else {
        8
    };
    while target_s > k
        && count_choice_combinations_capped(target_s, k, resolved_cap + 1) > resolved_cap
    {
        target_s -= 1;
    }
    let cap_total = target_s.max(k.min(n)).min(n);
    let mut kept_orig: SmallVec<[usize; 16]> = SmallVec::new();
    for &g in order.iter().take(cap_total.min(distinct_n)) {
        kept_orig.push(groups[g].0);
    }
    if kept_orig.len() < cap_total {
        for &g in &order {
            for &idx in groups[g].1.iter().skip(1) {
                if kept_orig.len() >= cap_total {
                    break;
                }
                kept_orig.push(idx);
            }
            if kept_orig.len() >= cap_total {
                break;
            }
        }
    }
    probed_sels.clear();
    scratch.probed_sels = probed_sels;
    scratch.marginal_scores = scores;
    scratch.marginal_order = order;

    let mut sub_req = req.clone();
    sub_req.candidates = kept_orig.iter().map(|&idx| req.candidates[idx]).collect();
    sub_req.max_count = sub_req.max_count.min(kept_orig.len());
    let mut sub_choices = std::mem::take(&mut scratch.choice_enum_bufs[0]);
    let mut enum_scratch = std::mem::take(&mut scratch.choice_enum_scratch);
    enumerate_canonical_choices_with_scratch(
        &mut scratch.choice_paused_states[0],
        content,
        &sub_req,
        resolved_cap,
        policy.accelerations.enable_multiset_choice_dedup,
        &mut enum_scratch,
        &mut sub_choices,
    );
    scratch.choice_enum_scratch = enum_scratch;
    for v in &mut sub_choices {
        for j in v.iter_mut() {
            *j = kept_orig[*j];
        }
        v.sort_unstable();
    }
    scratch.choice_enum_bufs[0] = sub_choices;
    true
}

#[allow(clippy::too_many_arguments)]
fn resolve_choices_recursive(
    scratch: &mut WorkerScratch,
    choices_so_far: &mut SmallVec<[SmallVec<[usize; 8]>; 2]>,
    depth: usize,
    task: &ActionTask,
    action: CompactAction,
    por_key: u64,
    is_delayed_or_end_turn: bool,
    is_turn_setup: bool,
    max_branches: usize,
    resolved_cap: usize,
    max_choice_depth: usize,
    defer_choice_probes: bool,
    static_ctx: &Arc<RootStaticContext>,
    content: &dyn ContentDispatcher,
    root_baseline: &RootEvalBaseline,
    policy: &SearchPolicy,
    profile: &SolverSearchProfile,
    cert: &ClosureCertificate,
    frozen_incumbent: &FrozenIncumbentBound,
    tt: &ShardedParetoTT,
    probe_cache: &mut ProbeCacheAccess<'_>,
    sensitivity: &CombatOrderSensitivity,
    allow_paid_optional_potions: bool,
    eval_mode: EvalMode,
) {
    if depth >= max_choice_depth || scratch.choice_resolved.len() >= resolved_cap {
        return;
    }
    scratch.ensure_choice_depth(depth, static_ctx);
    let need_screen = depth == 0
        && scratch.choice_paused_states[0]
            .pending
            .as_ref()
            .is_some_and(|p| {
                let req = &p.request;
                let n = req.candidates.len();
                n > 4
                    && total_choice_combos_exceeds(
                        n,
                        req.min_count,
                        req.max_count,
                        req.can_skip,
                        resolved_cap,
                    )
            });
    let screened = if need_screen {
        let req = scratch.choice_paused_states[0]
            .pending
            .as_ref()
            .expect("pending checked above")
            .request
            .clone();
        screen_high_branching_multi_pick(
            scratch,
            choices_so_far,
            task,
            action,
            por_key,
            is_delayed_or_end_turn,
            is_turn_setup,
            max_branches,
            resolved_cap,
            max_choice_depth,
            defer_choice_probes,
            static_ctx,
            content,
            root_baseline,
            policy,
            profile,
            cert,
            frozen_incumbent,
            tt,
            probe_cache,
            sensitivity,
            allow_paid_optional_potions,
            eval_mode,
            &req,
        )
    } else {
        false
    };
    if scratch.fatal_error.is_some() {
        return;
    }

    if !screened {
        let Some(req) = scratch.choice_paused_states[depth]
            .pending
            .as_ref()
            .map(|p| p.request.clone())
        else {
            return;
        };
        let mut buf = std::mem::take(&mut scratch.choice_enum_bufs[depth]);
        let mut enum_scratch = std::mem::take(&mut scratch.choice_enum_scratch);
        enumerate_canonical_choices_with_scratch(
            &mut scratch.choice_paused_states[depth],
            content,
            &req,
            resolved_cap,
            policy.accelerations.enable_multiset_choice_dedup,
            &mut enum_scratch,
            &mut buf,
        );
        scratch.choice_enum_scratch = enum_scratch;
        scratch.choice_enum_bufs[depth] = buf;
    }

    let choice_options = std::mem::take(&mut scratch.choice_enum_bufs[depth]);
    let num_options = choice_options.len();
    let is_expensive_hook_resume = scratch.choice_paused_states[depth]
        .pending
        .as_ref()
        .is_some_and(|p| {
            p.frames.iter().any(|f| {
                matches!(
                    f,
                    sts2_kernel::continuation::ContinuationFrame::AfterPlayerTurnStartResume { .. }
                        | sts2_kernel::continuation::ContinuationFrame::AfterAutoPostPlayResume { .. }
                        | sts2_kernel::continuation::ContinuationFrame::AfterAutoPostPlayPowerResume { .. }
                        | sts2_kernel::continuation::ContinuationFrame::EndTurnResume { .. }
                        | sts2_kernel::continuation::ContinuationFrame::EnemyTurnResume { .. }
                )
            })
        });
    for (opt_idx, sel) in choice_options.iter().enumerate() {
        if scratch.fatal_error.is_some() || scratch.choice_resolved.len() >= resolved_cap {
            break;
        }
        let est_total_options = scratch.choice_resolved.len() + (num_options - opt_idx);
        let defer_pack = if defer_choice_probes && est_total_options >= 2 {
            false
        } else if is_turn_setup
            || (is_expensive_hook_resume && est_total_options <= max_branches * 2)
        {
            false
        } else if est_total_options >= max_branches * 2 {
            true
        } else if est_total_options > max_branches {
            scratch.choice_resolved.len() >= max_branches
        } else {
            false
        };
        if depth > 0 && opt_idx + 1 == num_options {
            std::mem::swap(&mut scratch.choice_paused_states[depth], &mut scratch.state);
        } else {
            clone_battle_state_into(&scratch.choice_paused_states[depth], &mut scratch.state);
        }
        scratch.choice_replay_attempts += 1;
        scratch.transitions += 1;
        let outcome = match CombatEngine::resume_choice(&mut scratch.state, content, sel) {
            Ok(outcome) => outcome,
            Err(err) => {
                if should_abort_search_on_kernel_error(&err) {
                    scratch.record_fatal_error(err);
                    break;
                }
                continue;
            }
        };
        choices_so_far.push(sel.clone());
        match outcome {
            StepOutcome::Completed | StepOutcome::CombatEnded { .. } => {
                stage_resolved_choice(
                    scratch,
                    task,
                    action,
                    por_key,
                    choices_so_far.as_slice(),
                    defer_pack,
                    content,
                    root_baseline,
                    policy,
                    profile,
                    cert,
                    frozen_incumbent,
                    tt,
                    sensitivity,
                    allow_paid_optional_potions,
                    eval_mode,
                );
            }
            StepOutcome::NeedsChoice(_) => {
                if depth + 1 < max_choice_depth
                    && allows_brightest_flame(
                        policy.brightest_flame_max_hp_loss_limit,
                        root_baseline.initial_brightest_flame_max_hp_spent,
                        root_baseline.brightest_flame_spent(&scratch.state),
                    )
                {
                    scratch.ensure_choice_depth(depth + 1, static_ctx);
                    clone_battle_state_into(
                        &scratch.state,
                        &mut scratch.choice_paused_states[depth + 1],
                    );
                    resolve_choices_recursive(
                        scratch,
                        choices_so_far,
                        depth + 1,
                        task,
                        action,
                        por_key,
                        is_delayed_or_end_turn,
                        is_turn_setup,
                        max_branches,
                        resolved_cap,
                        max_choice_depth,
                        defer_choice_probes,
                        static_ctx,
                        content,
                        root_baseline,
                        policy,
                        profile,
                        cert,
                        frozen_incumbent,
                        tt,
                        probe_cache,
                        sensitivity,
                        allow_paid_optional_potions,
                        eval_mode,
                    );
                }
            }
        }
        choices_so_far.pop();
    }
    scratch.choice_enum_bufs[depth] = choice_options;
}

#[cfg(any(debug_assertions, test))]
#[inline]
fn compare_resolved_choice(
    a: &ResolvedChoiceCandidate,
    b: &ResolvedChoiceCandidate,
    criterion: usize,
) -> std::cmp::Ordering {
    let score_of = |c: &ResolvedChoiceCandidate| -> f64 {
        c.full_eval
            .as_ref()
            .map_or(c.base_score, |f| f.beam_rank_score)
    };
    let invest_of = |c: &ResolvedChoiceCandidate| -> i64 {
        c.full_eval.as_ref().map_or(0, |f| {
            (f.board_investment as i64) * 16_000 + (f.setup_score as i64) * 18_000
        })
    };
    let opt_of = |c: &ResolvedChoiceCandidate| -> i32 {
        (c.fast_eval.energy as i32).max(0)
            + (c.fast_eval.stars as i32).max(0)
            + c.full_eval
                .as_ref()
                .map_or(0, |f| f.optionality_cards)
    };

    match criterion {
        1 => {
            let cmp = score_of(b).total_cmp(&score_of(a));
            if !cmp.is_eq() {
                return cmp;
            }
        }
        2 => {
            let cmp = a.lookahead_hp_lost.cmp(&b.lookahead_hp_lost);
            if !cmp.is_eq() {
                return cmp;
            }
            let cmp = a
                .fast_eval
                .cumulative_hp_lost
                .cmp(&b.fast_eval.cumulative_hp_lost);
            if !cmp.is_eq() {
                return cmp;
            }
        }
        3 => {
            let a_look_inv = (a.lookahead_board_investment as i64) * 16_000
                + (a.lookahead_setup_score as i64) * 18_000;
            let b_look_inv = (b.lookahead_board_investment as i64) * 16_000
                + (b.lookahead_setup_score as i64) * 18_000;
            let cmp = b_look_inv.cmp(&a_look_inv);
            if !cmp.is_eq() {
                return cmp;
            }
            let cmp = invest_of(b).cmp(&invest_of(a));
            if !cmp.is_eq() {
                return cmp;
            }
            let cmp = opt_of(b).cmp(&opt_of(a));
            if !cmp.is_eq() {
                return cmp;
            }
        }
        _ => {}
    }

    b.lookahead_victory
        .cmp(&a.lookahead_victory)
        .then_with(|| b.has_lookahead.cmp(&a.has_lookahead))
        .then_with(|| {
            if a.has_lookahead && b.has_lookahead {
                b.lookahead_score.total_cmp(&a.lookahead_score)
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .then_with(|| score_of(b).total_cmp(&score_of(a)))
        .then_with(|| a.state_hash.cmp(&b.state_hash))
}

#[derive(Clone, Copy)]
struct ChoiceSortKey {
    state_hash: StateHash,
    lookahead_score: f64,
    beam_score: f64,
    lookahead_hp_lost: i32,
    cum_hp_lost: i16,
    lookahead_invest: i64,
    eval_invest: i64,
    opt: i32,
    effective_defense: i32,
    flags: u8, // bit 0 = lookahead_victory, bit 1 = has_lookahead, bit 2 = static_won, bit 3 = static_defeat
}

impl ChoiceSortKey {
    #[inline(always)]
    fn from_candidate(c: &ResolvedChoiceCandidate) -> Self {
        let (beam_score, eval_invest, opt_cards, effective_defense) = match c.full_eval.as_ref() {
            Some(f) => (
                f.beam_rank_score,
                (f.board_investment as i64) * 16_000 + (f.setup_score as i64) * 18_000,
                f.optionality_cards,
                f.effective_defense,
            ),
            None => (c.base_score, 0, 0, 0),
        };
        let opt = (c.fast_eval.energy as i32).max(0) + (c.fast_eval.stars as i32).max(0) + opt_cards;
        let static_won = c.fast_eval.won();
        let static_defeat = c.fast_eval.player_dead() || c.fast_eval.projected_player_hp <= 0;
        Self {
            state_hash: c.state_hash,
            lookahead_score: c.lookahead_score,
            beam_score,
            lookahead_hp_lost: c.lookahead_hp_lost,
            cum_hp_lost: c.fast_eval.cumulative_hp_lost,
            lookahead_invest: (c.lookahead_board_investment as i64) * 16_000
                + (c.lookahead_setup_score as i64) * 18_000,
            eval_invest,
            opt,
            effective_defense,
            flags: u8::from(c.lookahead_victory)
                | (u8::from(c.has_lookahead) << 1)
                | (u8::from(static_won) << 2)
                | (u8::from(static_defeat) << 3),
        }
    }
}

#[inline(always)]
fn compare_choice_static_key(
    a: &ChoiceSortKey,
    b: &ChoiceSortKey,
    criterion: usize,
) -> std::cmp::Ordering {
    let a_won = (a.flags & 4) != 0;
    let b_won = (b.flags & 4) != 0;
    let cmp = b_won.cmp(&a_won);
    if !cmp.is_eq() {
        return cmp;
    }
    match criterion {
        1 => {
            let a_def = (a.flags & 8) != 0;
            let b_def = (b.flags & 8) != 0;
            let cmp = a_def.cmp(&b_def);
            if !cmp.is_eq() {
                return cmp;
            }
            let cmp = a.cum_hp_lost.cmp(&b.cum_hp_lost);
            if !cmp.is_eq() {
                return cmp;
            }
            let cmp = b.effective_defense.cmp(&a.effective_defense);
            if !cmp.is_eq() {
                return cmp;
            }
        }
        2 => {
            let cmp = b.eval_invest.cmp(&a.eval_invest);
            if !cmp.is_eq() {
                return cmp;
            }
            let cmp = b.opt.cmp(&a.opt);
            if !cmp.is_eq() {
                return cmp;
            }
        }
        _ => {}
    }
    b.beam_score
        .total_cmp(&a.beam_score)
        .then_with(|| a.state_hash.cmp(&b.state_hash))
}

#[inline(always)]
fn compare_choice_sort_key(
    a: &ChoiceSortKey,
    b: &ChoiceSortKey,
    criterion: usize,
) -> std::cmp::Ordering {
    match criterion {
        1 => {
            let cmp = b.beam_score.total_cmp(&a.beam_score);
            if !cmp.is_eq() {
                return cmp;
            }
        }
        2 => {
            let cmp = a.lookahead_hp_lost.cmp(&b.lookahead_hp_lost);
            if !cmp.is_eq() {
                return cmp;
            }
            let cmp = a.cum_hp_lost.cmp(&b.cum_hp_lost);
            if !cmp.is_eq() {
                return cmp;
            }
        }
        3 => {
            let cmp = b.lookahead_invest.cmp(&a.lookahead_invest);
            if !cmp.is_eq() {
                return cmp;
            }
            let cmp = b.eval_invest.cmp(&a.eval_invest);
            if !cmp.is_eq() {
                return cmp;
            }
            let cmp = b.opt.cmp(&a.opt);
            if !cmp.is_eq() {
                return cmp;
            }
        }
        _ => {}
    }

    let b_vic = (b.flags & 1) != 0;
    let a_vic = (a.flags & 1) != 0;
    let b_has = (b.flags & 2) != 0;
    let a_has = (a.flags & 2) != 0;
    b_vic
        .cmp(&a_vic)
        .then_with(|| b_has.cmp(&a_has))
        .then_with(|| {
            if a_has && b_has {
                b.lookahead_score.total_cmp(&a.lookahead_score)
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .then_with(|| b.beam_score.total_cmp(&a.beam_score))
        .then_with(|| a.state_hash.cmp(&b.state_hash))
}

fn select_depth0_representatives_from_sorted_pool(
    resolved: &[ResolvedChoiceCandidate],
    choices: &ChoiceArena,
    sorted_pool: &[usize],
    marks: &mut [bool],
    max_take: usize,
    dst: &mut [usize],
    dst_len: &mut usize,
) {
    if max_take == 0 || sorted_pool.is_empty() {
        return;
    }
    let mut seen: SmallVec<[(u64, u32, u8); 64]> = SmallVec::new();
    let mut tmp_a: SmallVec<[usize; 16]> = SmallVec::new();
    let mut tmp_b: SmallVec<[usize; 16]> = SmallVec::new();

    for &idx in sorted_pool {
        if seen.len() >= max_take {
            break;
        }
        if marks[idx] {
            continue;
        }
        let cand = &resolved[idx];
        let mut dup = false;
        for &(sig, off_b, len_b) in &seen {
            if sig != cand.depth0_sig {
                continue;
            }
            if cand.choices_len == 0 && len_b == 0 {
                dup = true;
                break;
            }
            if cand.choices_len > 0 && len_b > 0 {
                choices.copy_step_indices(cand.choices_offset, 0, &mut tmp_a);
                choices.copy_step_indices(off_b, 0, &mut tmp_b);
                if tmp_a == tmp_b {
                    dup = true;
                    break;
                }
            }
        }
        if dup {
            continue;
        }
        seen.push((cand.depth0_sig, cand.choices_offset, cand.choices_len));
        marks[idx] = true;
        dst[*dst_len] = idx;
        *dst_len += 1;
    }
}

fn pre_filter_static_probe_candidates(
    resolved: &[ResolvedChoiceCandidate],
    choices: &ChoiceArena,
    probe_budget: usize,
    probe_out: &mut Vec<usize>,
) {
    let resolved_count = resolved.len();
    probe_out.clear();
    if probe_budget >= resolved_count {
        probe_out.extend(0..resolved_count);
        return;
    }
    probe_out.resize(probe_budget, 0);
    let mut keys: [std::mem::MaybeUninit<ChoiceSortKey>; TURN_SETUP_CHOICE_RESOLVED_CAPACITY] =
        [const { std::mem::MaybeUninit::uninit() }; TURN_SETUP_CHOICE_RESOLVED_CAPACITY];
    debug_assert!(resolved_count <= TURN_SETUP_CHOICE_RESOLVED_CAPACITY);
    for (i, c) in resolved.iter().enumerate() {
        keys[i].write(ChoiceSortKey::from_candidate(c));
    }
    // SAFETY: `keys[..resolved_count]` was just initialized above.
    let keys = unsafe {
        std::slice::from_raw_parts(keys.as_ptr().cast::<ChoiceSortKey>(), resolved_count)
    };
    let mut static_order_buf = [0usize; TURN_SETUP_CHOICE_RESOLVED_CAPACITY];
    let static_order = &mut static_order_buf[..resolved_count];
    for (i, slot) in static_order.iter_mut().enumerate() {
        *slot = i;
    }
    static_order.sort_unstable_by(|&ia, &ib| {
        compare_choice_static_key(&keys[ia], &keys[ib], 0).then_with(|| ia.cmp(&ib))
    });
    let mut marks = [false; TURN_SETUP_CHOICE_RESOLVED_CAPACITY];
    let mut written = 0usize;

    let min_niche_reserve = ((probe_budget * 35) / 100).max(6);
    let depth0_probe_cap = probe_budget.saturating_sub(min_niche_reserve).min(probe_budget);
    select_depth0_representatives_from_sorted_pool(
        resolved,
        choices,
        static_order,
        &mut marks[..resolved_count],
        depth0_probe_cap,
        probe_out,
        &mut written,
    );

    let remaining = probe_budget.saturating_sub(written);
    if remaining > 0 {
        let surv_quota = if remaining >= 3 {
            ((remaining * 25 + 50) / 100).max(1)
        } else {
            0
        };
        let inv_quota = if remaining >= 2 {
            ((remaining * 25 + 50) / 100).max(1)
        } else {
            0
        };
        let eval_quota = remaining.saturating_sub(surv_quota + inv_quota);

        for &(take, criterion) in &[
            (eval_quota, 0usize),
            (surv_quota, 1usize),
            (inv_quota, 2usize),
            (probe_budget, 0usize),
        ] {
            for _ in 0..take {
                if written >= probe_budget {
                    break;
                }
                let mut best_i: Option<usize> = None;
                for i in 0..resolved_count {
                    if marks[i] {
                        continue;
                    }
                    if best_i.is_none_or(|bi| {
                        compare_choice_static_key(&keys[i], &keys[bi], criterion)
                            .then_with(|| i.cmp(&bi))
                            .is_lt()
                    }) {
                        best_i = Some(i);
                    }
                }
                let Some(bi) = best_i else { break };
                marks[bi] = true;
                probe_out[written] = bi;
                written += 1;
            }
        }
    }
    probe_out.truncate(written);
}

fn select_choice_indices_from_pool(
    resolved: &[ResolvedChoiceCandidate],
    choices: &ChoiceArena,
    probed_pool: &[usize],
    max_branches: usize,
    selected_indices: &mut [usize; TURN_SETUP_CHOICE_RESOLVED_CAPACITY],
) -> usize {
    let resolved_count = resolved.len();
    let mut keys: [std::mem::MaybeUninit<ChoiceSortKey>; TURN_SETUP_CHOICE_RESOLVED_CAPACITY] =
        [const { std::mem::MaybeUninit::uninit() }; TURN_SETUP_CHOICE_RESOLVED_CAPACITY];
    debug_assert!(resolved_count <= TURN_SETUP_CHOICE_RESOLVED_CAPACITY);
    for (i, c) in resolved[..resolved_count].iter().enumerate() {
        keys[i].write(ChoiceSortKey::from_candidate(c));
    }
    // SAFETY: `keys[..resolved_count]` was just initialized above.
    let keys = unsafe {
        std::slice::from_raw_parts(keys.as_ptr().cast::<ChoiceSortKey>(), resolved_count)
    };

    let mut selected_len = 0usize;
    if resolved_count <= max_branches {
        for i in 0..resolved_count {
            selected_indices[selected_len] = i;
            selected_len += 1;
        }
        selected_indices[..selected_len].sort_unstable_by(|&ia, &ib| {
            let ord = compare_choice_sort_key(&keys[ia], &keys[ib], 0).then_with(|| ia.cmp(&ib));
            #[cfg(debug_assertions)]
            debug_assert_eq!(
                compare_choice_sort_key(&keys[ia], &keys[ib], 0),
                compare_resolved_choice(&resolved[ia], &resolved[ib], 0)
            );
            ord
        });
        return selected_len;
    }

    let cap = max_branches.min(probed_pool.len()).min(resolved_count);
    if cap == 0 {
        return 0;
    }

    let mut sorted_pool_buf = [0usize; TURN_SETUP_CHOICE_RESOLVED_CAPACITY];
    let sorted_pool = &mut sorted_pool_buf[..probed_pool.len()];
    sorted_pool.copy_from_slice(probed_pool);
    sorted_pool.sort_unstable_by(|&ia, &ib| {
        let ord = compare_choice_sort_key(&keys[ia], &keys[ib], 0).then_with(|| ia.cmp(&ib));
        #[cfg(debug_assertions)]
        debug_assert_eq!(
            compare_choice_sort_key(&keys[ia], &keys[ib], 0),
            compare_resolved_choice(&resolved[ia], &resolved[ib], 0)
        );
        ord
    });

    let mut marks = [false; TURN_SETUP_CHOICE_RESOLVED_CAPACITY];
    let depth0_cap = cap / 2;
    if depth0_cap > 0 {
        select_depth0_representatives_from_sorted_pool(
            resolved,
            choices,
            sorted_pool,
            &mut marks[..resolved_count],
            depth0_cap,
            selected_indices,
            &mut selected_len,
        );
    }

    let remaining = cap.saturating_sub(selected_len);
    if remaining > 0 {
        let primary_q = ((remaining * 55 + 50) / 100).max(1);
        let immediate_q = if remaining >= 4 {
            ((remaining * 15) / 100).max(1)
        } else {
            0
        };
        let survival_q = if remaining >= 3 {
            ((remaining * 15) / 100).max(1)
        } else {
            0
        };
        let invest_q = remaining.saturating_sub(primary_q + immediate_q + survival_q);

        for &(take, criterion) in &[
            (primary_q, 0usize),
            (immediate_q, 1usize),
            (survival_q, 2usize),
            (invest_q, 3usize),
            (cap, 0usize),
        ] {
            for _ in 0..take {
                if selected_len >= cap {
                    break;
                }
                let mut best_i: Option<usize> = None;
                for &idx in &*sorted_pool {
                    if marks[idx] {
                        continue;
                    }
                    if best_i.is_none_or(|bi| {
                        let ord = compare_choice_sort_key(&keys[idx], &keys[bi], criterion);
                        #[cfg(debug_assertions)]
                        debug_assert_eq!(
                            ord,
                            compare_resolved_choice(&resolved[idx], &resolved[bi], criterion)
                        );
                        ord.then_with(|| idx.cmp(&bi)).is_lt()
                    }) {
                        best_i = Some(idx);
                    }
                }
                let Some(bi) = best_i else { break };
                marks[bi] = true;
                selected_indices[selected_len] = bi;
                selected_len += 1;
            }
        }
    }

    selected_indices[..selected_len].sort_unstable_by(|&ia, &ib| {
        compare_choice_sort_key(&keys[ia], &keys[ib], 0).then_with(|| ia.cmp(&ib))
    });
    selected_len
}

#[allow(clippy::too_many_arguments)]
fn emit_selected_choices(
    parent_id: NodeId,
    parent_rank: u32,
    action_ord: u32,
    resolved: &[ResolvedChoiceCandidate],
    selected: &[usize],
    src_slab: &LocalStateSlab,
    src_choices: &ChoiceArena,
    dst_slab: &mut LocalStateSlab,
    dst_choices: &mut ChoiceArena,
    dst_candidates: &mut Vec<WorkerCandidate>,
    dst_shards: &mut Vec<u16>,
    dst_fast_meta: &mut Vec<WorkerCandFastMeta>,
    paused_base: Option<&BattleState>,
    mut replay_state: Option<&mut BattleState>,
    mut pack_buf: Option<&mut Vec<u8>>,
    content: &dyn ContentDispatcher,
) {
    for (choice_ord, &idx) in selected.iter().enumerate() {
        let cand = resolved[idx];
        let (state_slab_idx, state_off, state_len) = if cand.is_terminal {
            (0, 0, 0)
        } else if cand.state_len > 0 {
            let bytes = src_slab.get(cand.state_slab_idx, cand.state_off, cand.state_len);
            dst_slab.alloc_slice(bytes)
        } else if let (Some(base), Some(rstate), Some(pbuf)) = (
            paused_base,
            replay_state.as_deref_mut(),
            pack_buf.as_deref_mut(),
        ) {
            let ok = replay_deferred_choice_state_from_arena(
                base,
                rstate,
                src_choices,
                cand.choices_offset,
                cand.choices_len,
                cand.has_extra_end_turn,
                content,
            );
            debug_assert!(ok, "deferred choice replay failed in commit_resolved_choices");
            if !ok {
                continue;
            }
            #[cfg(debug_assertions)]
            debug_assert_eq!(
                hash_canonical_key(rstate),
                cand.state_hash,
                "deferred choice replay state_hash diverged"
            );
            pack_state(rstate, pbuf);
            dst_slab.alloc_slice(pbuf)
        } else {
            debug_assert!(false, "missing packed bytes for deferred choice batch");
            continue;
        };
        let (choices_offset, choices_len) = dst_choices.extend_from_arena_slice(
            src_choices,
            cand.choices_offset,
            cand.choices_len,
        );
        dst_shards.push(if cand.is_terminal {
            u16::MAX
        } else {
            cand.state_hash.shard_index() as u16
        });
        let fast_beam_score = cand
            .full_eval
            .as_ref()
            .map_or(cand.base_score, |f| f.beam_rank_score);
        dst_fast_meta.push(WorkerCandFastMeta {
            flags: u8::from(cand.is_terminal) | (u8::from(cand.turn_completed) << 1),
            potion_count: cand.potion_count.min(u8::MAX as u16) as u8,
            explicit_potion_count: cand.explicit_potion_count.min(u8::MAX as u16) as u8,
            _pad: [0; 5],
            beam_score: fast_beam_score,
        });
        dst_candidates.push(WorkerCandidate {
            sort_key: (parent_rank, action_ord, choice_ord as u32),
            parent_id,
            action: cand.action,
            por_key: cand.por_key,
            has_extra_end_turn: cand.has_extra_end_turn,
            choices_offset,
            choices_len,
            state_slab_idx,
            state_off,
            state_len,
            state_hash: cand.state_hash,
            turn: cand.turn,
            turn_completed: cand.turn_completed,
            action_count: cand.action_count,
            potion_count: cand.potion_count,
            explicit_potion_count: cand.explicit_potion_count,
            optional_paid_potion_count: cand.optional_paid_potion_count,
            automatic_potion_count: cand.automatic_potion_count,
            used_potion_slot_mask: cand.used_potion_slot_mask,
            potion_strategic_cost: cand.potion_strategic_cost,
            optional_paid_potion_cost: cand.optional_paid_potion_cost,
            ambergris_count: cand.ambergris_count,
            forced_used_mask: cand.forced_used_mask,
            forced_ambergris_count: cand.forced_ambergris_count,
            is_terminal: cand.is_terminal,
            base_score: cand.base_score,
            fast_eval: cand.fast_eval,
            full_eval: cand.full_eval,
            rare_eval: cand.rare_eval,
            label: cand.label,
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn commit_resolved_choices(
    scratch: &mut WorkerScratch,
    task: &ActionTask,
    task_idx: usize,
    defer_large_choice_probes: bool,
    turn_boundary_rollouts_enabled: bool,
    first_pile: PileType,
    is_end_turn: bool,
    is_turn_setup: bool,
    max_branches: usize,
    static_ctx: &Arc<RootStaticContext>,
    content: &dyn ContentDispatcher,
    root_baseline: &RootEvalBaseline,
    profile: &SolverSearchProfile,
    eval_mode: EvalMode,
    probe_cache: &mut ProbeCacheAccess<'_>,
) -> bool {
    let resolved_count = scratch.choice_resolved.len();
    if resolved_count == 0 {
        return false;
    }

    let max_prefix_len = scratch.choice_resolved[..resolved_count]
        .iter()
        .map(|c| c.choices_len as usize)
        .max()
        .unwrap_or(0);
    let is_start_turn = task.parent_turn == root_baseline.start_turn_number;
    let effective_max_branches = if is_start_turn && max_prefix_len >= 2 {
        max_branches.max(36)
    } else {
        max_branches
    };
    let cap = resolved_count.min(effective_max_branches);
    let needs_pruning = resolved_count > cap;
    if needs_pruning {
        scratch.branch_truncations += 1;
    }

    let is_delayed_pile_choice = matches!(
        first_pile,
        PileType::Draw | PileType::Discard | PileType::Exhaust
    );
    let run_unpruned_probe = !needs_pruning
        && resolved_count >= 2
        && !is_turn_setup
        && ((is_start_turn && is_delayed_pile_choice && !is_end_turn && resolved_count <= 12)
            || (is_end_turn && !turn_boundary_rollouts_enabled));
    let run_mini_rollout = needs_pruning || run_unpruned_probe;

    let mut probed_pool: Vec<usize> = Vec::new();

    if run_mini_rollout {
        let probe_max_turns_ahead = if !needs_pruning {
            1
        } else if first_pile == PileType::Hand {
            0
        } else {
            1
        };
        if !needs_pruning {
            probed_pool.extend(0..resolved_count);
        } else {
            let probe_budget = resolved_count.min((effective_max_branches * 2).max(16));
            if probe_budget >= resolved_count {
                probed_pool.extend(0..resolved_count);
            } else {
                pre_filter_static_probe_candidates(
                    &scratch.choice_resolved[..resolved_count],
                    &scratch.choice_staging_choices,
                    probe_budget,
                    &mut probed_pool,
                );
            }
        }

        let unprobed_count = if defer_large_choice_probes {
            probed_pool
                .iter()
                .filter(|&&i| {
                    let c = &scratch.choice_resolved[i];
                    !c.is_terminal && !c.has_lookahead
                })
                .count()
        } else {
            0
        };
        if unprobed_count >= 2 {
            let mut missing_bytes = false;
            for &i in &probed_pool {
                let c = &scratch.choice_resolved[i];
                if !c.is_terminal && !c.has_lookahead && c.state_len == 0 {
                    missing_bytes = true;
                    break;
                }
            }
            if missing_bytes {
                for &i in &probed_pool {
                    if scratch.choice_resolved[i].is_terminal
                        || scratch.choice_resolved[i].has_lookahead
                        || scratch.choice_resolved[i].state_len > 0
                    {
                        continue;
                    }
                    let (ch_off, ch_len, has_extra_end) = {
                        let c = &scratch.choice_resolved[i];
                        (c.choices_offset, c.choices_len, c.has_extra_end_turn)
                    };
                    if replay_deferred_choice_state_from_arena(
                        &scratch.choice_paused_states[0],
                        &mut scratch.state,
                        &scratch.choice_staging_choices,
                        ch_off,
                        ch_len,
                        has_extra_end,
                        content,
                    ) {
                        pack_state(&scratch.state, &mut scratch.pack_buf);
                        let (si, so, sl) = scratch.choice_slab.alloc_slice(&scratch.pack_buf);
                        let c = &mut scratch.choice_resolved[i];
                        c.state_slab_idx = si;
                        c.state_off = so;
                        c.state_len = sl;
                    }
                }
            }
            let resolved = std::mem::take(&mut scratch.choice_resolved);
            let slab = std::mem::take(&mut scratch.choice_slab);
            let choices = std::mem::take(&mut scratch.choice_staging_choices);
            let probe_results = vec![None; resolved.len()];
            scratch.deferred_batches.push(DeferredChoiceBatch {
                task_idx,
                parent_rank: task.parent_rank,
                action_ord: task.action_ord,
                parent_id: task.parent_id,
                max_branches: cap,
                probe_max_turns_ahead,
                probed_pool,
                selected_indices: SmallVec::new(),
                resolved,
                slab,
                choices,
                probe_results,
            });
            return true;
        }

        for &i in &probed_pool {
            if scratch.choice_resolved[i].has_lookahead {
                let c = &mut scratch.choice_resolved[i];
                if !c.turn_completed {
                    let encoded = c.lookahead_score;
                    if let Some(fe) = c.full_eval.as_mut() {
                        fe.has_choice_lookahead = true;
                        fe.choice_lookahead_score = Some(encoded);
                    }
                }
                continue;
            }
            let (
                is_term,
                slab_idx,
                off,
                len,
                state_hash,
                opt_cost,
                opt_count,
                action_count,
                ch_off,
                ch_len,
                has_extra_end,
            ) = {
                let c = &scratch.choice_resolved[i];
                (
                    c.is_terminal,
                    c.state_slab_idx,
                    c.state_off,
                    c.state_len,
                    c.state_hash,
                    c.optional_paid_potion_cost,
                    c.optional_paid_potion_count,
                    c.action_count,
                    c.choices_offset,
                    c.choices_len,
                    c.has_extra_end_turn,
                )
            };
            let probe = if is_term {
                let c = &scratch.choice_resolved[i];
                ChoiceProbeResult {
                    score: c.terminal_probe_score,
                    hp_lost: c.fast_eval.cumulative_hp_lost as i32,
                    setup_score: c.full_eval.as_ref().map_or(0, |f| f.setup_score),
                    board_investment: c.full_eval.as_ref().map_or(0, |f| f.board_investment),
                    strategy_goal_hp_credit: c.fast_eval.strategy_goal_hp_credit as i32,
                    strategic_hp_credit: c.fast_eval.strategic_hp_credit as i32,
                    is_victory: c.fast_eval.won(),
                    is_defeat: c.fast_eval.player_dead() || c.fast_eval.projected_player_hp <= 0,
                    is_event_defeat: c.fast_eval.is_event_defeat(),
                }
            } else if len > 0 {
                let state_bytes = scratch.choice_slab.get(slab_idx, off, len);
                // SAFETY: `state_bytes` was written by `pack_state` in this process.
                if unsafe { unpack_state(state_bytes, static_ctx, &mut scratch.state) }.is_err() {
                    continue;
                }
                let Some(probe) = evaluate_choice_probe_in_place(
                    &mut scratch.choice_probe_ws,
                    &mut scratch.state,
                    state_bytes,
                    state_hash,
                    probe_cache,
                    static_ctx,
                    content,
                    root_baseline,
                    profile,
                    eval_mode,
                    probe_max_turns_ahead,
                    opt_cost,
                    opt_count as usize,
                    action_count,
                ) else {
                    continue;
                };
                probe
            } else {
                if !replay_deferred_choice_state_from_arena(
                    &scratch.choice_paused_states[0],
                    &mut scratch.state,
                    &scratch.choice_staging_choices,
                    ch_off,
                    ch_len,
                    has_extra_end,
                    content,
                ) {
                    continue;
                }
                let paused_base = &scratch.choice_paused_states[0];
                let arena_ref = &scratch.choice_staging_choices;
                let Some(probe) = evaluate_choice_probe_in_place_with_restore(
                    &mut scratch.choice_probe_ws,
                    &mut scratch.state,
                    |st| {
                        replay_deferred_choice_state_from_arena(
                            paused_base,
                            st,
                            arena_ref,
                            ch_off,
                            ch_len,
                            has_extra_end,
                            content,
                        )
                    },
                    state_hash,
                    probe_cache,
                    content,
                    root_baseline,
                    profile,
                    eval_mode,
                    probe_max_turns_ahead,
                    opt_cost,
                    opt_count as usize,
                    action_count,
                ) else {
                    continue;
                };
                probe
            };
            let encoded = encode_choice_probe_rank_score_with_policy(&probe, root_baseline);
            let c = &mut scratch.choice_resolved[i];
            c.lookahead_score = encoded;
            c.lookahead_hp_lost = probe.hp_lost;
            c.lookahead_board_investment = probe.board_investment;
            c.lookahead_setup_score = probe.setup_score;
            c.lookahead_victory = probe.is_victory;
            c.has_lookahead = true;
            if !c.turn_completed {
                if let Some(fe) = c.full_eval.as_mut() {
                    fe.has_choice_lookahead = true;
                    fe.choice_lookahead_score = Some(encoded);
                }
            }
        }
    } else {
        probed_pool.extend(0..resolved_count);
    }

    let mut selected_indices = [0usize; TURN_SETUP_CHOICE_RESOLVED_CAPACITY];
    let selected_len = select_choice_indices_from_pool(
        &scratch.choice_resolved[..resolved_count],
        &scratch.choice_staging_choices,
        &probed_pool,
        cap,
        &mut selected_indices,
    );
    emit_selected_choices(
        task.parent_id,
        task.parent_rank,
        task.action_ord,
        &scratch.choice_resolved,
        &selected_indices[..selected_len],
        &scratch.choice_slab,
        &scratch.choice_staging_choices,
        &mut scratch.slab,
        &mut scratch.staged_choices,
        &mut scratch.candidates,
        &mut scratch.cand_shards,
        &mut scratch.cand_fast_meta,
        Some(&scratch.choice_paused_states[0]),
        Some(&mut scratch.state),
        Some(&mut scratch.pack_buf),
        content,
    );
    scratch.choice_resolved.clear();
    false
}

#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn resolve_deferred_choice_batches_on_pool(
    dop: usize,
    pool: Option<&rayon::ThreadPool>,
    scratches: &mut [WorkerScratch],
    deferred_batches: &mut Vec<DeferredChoiceBatch>,
    probe_jobs: &mut Vec<ProbeJob>,
    probe_cache: &ChoiceProbeCache,
    static_ctx: &Arc<RootStaticContext>,
    content: &(dyn ContentDispatcher + Sync),
    root_baseline: &RootEvalBaseline,
    profile: &SolverSearchProfile,
    eval_mode: EvalMode,
    cancel: &AtomicBool,
    commit_slices: &[AtomicU64],
    write_commit_slices: bool,
) {
    deferred_batches.clear();
    for s in scratches.iter_mut() {
        if !s.deferred_batches.is_empty() {
            deferred_batches.append(&mut s.deferred_batches);
        }
    }
    if deferred_batches.is_empty() {
        return;
    }

    probe_jobs.clear();
    for (b_idx, batch) in deferred_batches.iter_mut().enumerate() {
        for &c_idx in &batch.probed_pool {
            let c = &batch.resolved[c_idx];
            if c.is_terminal {
                batch.probe_results[c_idx] = Some(ChoiceProbeResult {
                    score: c.terminal_probe_score,
                    hp_lost: c.fast_eval.cumulative_hp_lost as i32,
                    setup_score: c.full_eval.as_ref().map_or(0, |f| f.setup_score),
                    board_investment: c.full_eval.as_ref().map_or(0, |f| f.board_investment),
                    strategy_goal_hp_credit: c.fast_eval.strategy_goal_hp_credit as i32,
                    strategic_hp_credit: c.fast_eval.strategic_hp_credit as i32,
                    is_victory: c.fast_eval.won(),
                    is_defeat: c.fast_eval.player_dead() || c.fast_eval.projected_player_hp <= 0,
                    is_event_defeat: c.fast_eval.is_event_defeat(),
                });
            } else if !c.has_lookahead && c.state_len > 0 {
                probe_jobs.push(ProbeJob {
                    batch_idx: b_idx as u32,
                    cand_idx: c_idx as u32,
                    state_slab_idx: c.state_slab_idx,
                    state_off: c.state_off,
                    state_len: c.state_len,
                    state_hash: c.state_hash,
                    opt_cost: c.optional_paid_potion_cost,
                    opt_count: c.optional_paid_potion_count as usize,
                    action_count: c.action_count,
                });
            }
        }
    }

    if !probe_jobs.is_empty() {
        if let Some(p) = pool.filter(|_| dop > 1 && probe_jobs.len() >= 2 && scratches.len() >= 2)
        {
            let workers = dop.min(probe_jobs.len()).min(scratches.len());
            let jobs_ref = probe_jobs.as_slice();
            let batches_ref = deferred_batches.as_slice();
            let next_job = AtomicUsize::new(0);
            for scratch in &mut scratches[..workers] {
                scratch.deferred_probe_out.clear();
            }
            let (first_scratch, rest_scratches) =
                scratches[..workers].split_first_mut().expect("workers >= 2");
            let run_probe_worker = |scratch: &mut WorkerScratch| {
                let mut probe_access = ProbeCacheAccess::Shared(probe_cache);
                loop {
                    if cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    let j = next_job.fetch_add(1, Ordering::Relaxed);
                    if j >= jobs_ref.len() {
                        break;
                    }
                    let job = jobs_ref[j];
                    let batch = &batches_ref[job.batch_idx as usize];
                    let bytes = batch.slab.get(job.state_slab_idx, job.state_off, job.state_len);
                    let res = evaluate_choice_probe_packed(
                        &mut scratch.choice_probe_ws,
                        bytes,
                        job.state_hash,
                        &mut probe_access,
                        static_ctx,
                        content,
                        root_baseline,
                        profile,
                        eval_mode,
                        batch.probe_max_turns_ahead,
                        job.opt_cost,
                        job.opt_count,
                        job.action_count,
                    );
                    scratch.deferred_probe_out.push((j, res));
                }
            };
            let worker_fn = &run_probe_worker;
            p.scope(|scope| {
                for scratch in rest_scratches.iter_mut() {
                    scope.spawn(move |_| worker_fn(scratch));
                }
                worker_fn(first_scratch);
            });
            for scratch in &mut scratches[..workers] {
                for (j, res) in scratch.deferred_probe_out.drain(..) {
                    let job = probe_jobs[j];
                    deferred_batches[job.batch_idx as usize].probe_results[job.cand_idx as usize] =
                        res;
                }
            }
        } else {
            let mut probe_access = ProbeCacheAccess::Shared(probe_cache);
            for &job in probe_jobs.iter() {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                let batch = &deferred_batches[job.batch_idx as usize];
                let bytes = batch.slab.get(job.state_slab_idx, job.state_off, job.state_len);
                let res = evaluate_choice_probe_packed(
                    &mut scratches[0].choice_probe_ws,
                    bytes,
                    job.state_hash,
                    &mut probe_access,
                    static_ctx,
                    content,
                    root_baseline,
                    profile,
                    eval_mode,
                    batch.probe_max_turns_ahead,
                    job.opt_cost,
                    job.opt_count,
                    job.action_count,
                );
                deferred_batches[job.batch_idx as usize].probe_results[job.cand_idx as usize] = res;
            }
        }
    }

    let select_batch = |batch: &mut DeferredChoiceBatch| {
        for &c_idx in &batch.probed_pool {
            let c = &mut batch.resolved[c_idx];
            if c.has_lookahead {
                if !c.turn_completed {
                    let encoded = c.lookahead_score;
                    if let Some(fe) = c.full_eval.as_mut() {
                        fe.has_choice_lookahead = true;
                        fe.choice_lookahead_score = Some(encoded);
                    }
                }
                continue;
            }
            if let Some(probe) = &batch.probe_results[c_idx] {
                let encoded = encode_choice_probe_rank_score_with_policy(probe, root_baseline);
                c.lookahead_score = encoded;
                c.lookahead_hp_lost = probe.hp_lost;
                c.lookahead_board_investment = probe.board_investment;
                c.lookahead_setup_score = probe.setup_score;
                c.lookahead_victory = probe.is_victory;
                c.has_lookahead = true;
                if !c.turn_completed {
                    if let Some(fe) = c.full_eval.as_mut() {
                        fe.has_choice_lookahead = true;
                        fe.choice_lookahead_score = Some(encoded);
                    }
                }
            }
        }
        let mut selected_indices = [0usize; TURN_SETUP_CHOICE_RESOLVED_CAPACITY];
        let selected_len = select_choice_indices_from_pool(
            &batch.resolved,
            &batch.choices,
            &batch.probed_pool,
            batch.max_branches,
            &mut selected_indices,
        );
        batch.selected_indices.clear();
        batch
            .selected_indices
            .extend_from_slice(&selected_indices[..selected_len]);
    };

    if let Some(p) = pool.filter(|_| dop > 1 && deferred_batches.len() >= 2) {
        p.install(|| {
            deferred_batches.par_iter_mut().for_each(select_batch);
        });
    } else {
        for batch in deferred_batches.iter_mut() {
            select_batch(batch);
        }
    }

    if let Some(p) = pool.filter(|_| dop > 1 && deferred_batches.len() >= 2 && scratches.len() >= 2)
    {
        let workers = dop.min(deferred_batches.len()).min(scratches.len());
        let batches_ref = deferred_batches.as_slice();
        let next_batch = AtomicUsize::new(0);
        let (first_scratch, rest_scratches) =
            scratches[..workers].split_first_mut().expect("workers >= 2");
        let run_emit = |w_idx: usize, scratch: &mut WorkerScratch| {
            scratch.wave_dirty = true;
            loop {
                let b_idx = next_batch.fetch_add(1, Ordering::Relaxed);
                if b_idx >= batches_ref.len() {
                    break;
                }
                let batch = &batches_ref[b_idx];
                let c_start = scratch.candidates.len();
                emit_selected_choices(
                    batch.parent_id,
                    batch.parent_rank,
                    batch.action_ord,
                    &batch.resolved,
                    &batch.selected_indices,
                    &batch.slab,
                    &batch.choices,
                    &mut scratch.slab,
                    &mut scratch.staged_choices,
                    &mut scratch.candidates,
                    &mut scratch.cand_shards,
                    &mut scratch.cand_fast_meta,
                    None,
                    None,
                    None,
                    content,
                );
                let c_len = scratch.candidates.len() - c_start;
                if write_commit_slices && c_len > 0 {
                    commit_slices[batch.task_idx].store(
                        pack_parent_slice(w_idx, c_start, c_len),
                        Ordering::Relaxed,
                    );
                }
            }
        };
        let emit_ref = &run_emit;
        p.scope(|scope| {
            for (i, scratch) in rest_scratches.iter_mut().enumerate() {
                scope.spawn(move |_| emit_ref(i + 1, scratch));
            }
            emit_ref(0, first_scratch);
        });
    } else {
        let scratch = &mut scratches[0];
        scratch.wave_dirty = true;
        for batch in deferred_batches.iter() {
            let c_start = scratch.candidates.len();
            emit_selected_choices(
                batch.parent_id,
                batch.parent_rank,
                batch.action_ord,
                &batch.resolved,
                &batch.selected_indices,
                &batch.slab,
                &batch.choices,
                &mut scratch.slab,
                &mut scratch.staged_choices,
                &mut scratch.candidates,
                &mut scratch.cand_shards,
                &mut scratch.cand_fast_meta,
                None,
                None,
                None,
                content,
            );
            let c_len = scratch.candidates.len() - c_start;
            if write_commit_slices && c_len > 0 {
                commit_slices[batch.task_idx]
                    .store(pack_parent_slice(0, c_start, c_len), Ordering::Relaxed);
            }
        }
    }
    deferred_batches.clear();
}

#[allow(clippy::too_many_arguments)]
fn execute_action_from_parent(
    scratch: &mut WorkerScratch,
    parent_state: &BattleState,
    task: &ActionTask,
    task_idx: usize,
    defer_large_choice_probes: bool,
    turn_boundary_rollouts_enabled: bool,
    static_ctx: &Arc<RootStaticContext>,
    content: &dyn ContentDispatcher,
    root_baseline: &RootEvalBaseline,
    policy: &SearchPolicy,
    profile: &SolverSearchProfile,
    cert: &ClosureCertificate,
    frozen_incumbent: &FrozenIncumbentBound,
    tt: &ShardedParetoTT,
    probe_cache: &mut ProbeCacheAccess<'_>,
    sensitivity: &CombatOrderSensitivity,
    allow_paid_optional_potions: bool,
    eval_mode: EvalMode,
) -> bool {
    clone_battle_state_into(parent_state, &mut scratch.state);

    let (action, por_key) = match task.spec {
        ActionSpec::TurnSetup => CompactAction::new(
            CompactActionKind::TurnSetup,
            task.parent_turn,
            PorActionClass::Barrier,
        ),
        ActionSpec::PlayCard {
            card_handle,
            target_handle,
            por_class,
        } => CompactAction::new(
            CompactActionKind::PlayCard {
                card_handle: card_handle.0,
                target_handle: target_handle.map(|h| h.0),
            },
            task.parent_turn,
            por_class,
        ),
        ActionSpec::UsePotion {
            potion_slot,
            potion_id,
            target_handle,
            ..
        } => CompactAction::new(
            CompactActionKind::UsePotion {
                potion_slot: potion_slot as u16,
                potion_id,
                target_handle: target_handle.map(|h| h.0),
            },
            task.parent_turn,
            PorActionClass::Barrier,
        ),
        ActionSpec::EndTurn => CompactAction::new(
            CompactActionKind::EndTurn,
            task.parent_turn,
            PorActionClass::Barrier,
        ),
    };

    if matches!(task.spec, ActionSpec::TurnSetup) {
        let Some(req) = scratch.state.pending.as_ref().map(|p| p.request.clone()) else {
            return false;
        };
        let first_pile = req.source_pile;
        let (is_delayed_or_end_turn, max_branches, resolved_cap) =
            choice_probe_limits(profile, first_pile, false, true);
        let max_choice_depth = 16;
        scratch.begin_choice_tree();
        clone_battle_state_into(&scratch.state, &mut scratch.choice_paused_states[0]);
        let mut choices_so_far: SmallVec<[SmallVec<[usize; 8]>; 2]> = SmallVec::new();
        resolve_choices_recursive(
            scratch,
            &mut choices_so_far,
            0,
            task,
            action,
            por_key,
            is_delayed_or_end_turn,
            true,
            max_branches,
            resolved_cap,
            max_choice_depth,
            defer_large_choice_probes,
            static_ctx,
            content,
            root_baseline,
            policy,
            profile,
            cert,
            frozen_incumbent,
            tt,
            probe_cache,
            sensitivity,
            allow_paid_optional_potions,
            eval_mode,
        );
        if scratch.fatal_error.is_some() {
            return false;
        }
        return commit_resolved_choices(
            scratch,
            task,
            task_idx,
            defer_large_choice_probes,
            turn_boundary_rollouts_enabled,
            first_pile,
            false,
            true,
            max_branches,
            static_ctx,
            content,
            root_baseline,
            profile,
            eval_mode,
            probe_cache,
        );
    }

    let mut empty_q: SmallVec<[Vec<usize>; 2]> = SmallVec::new();
    scratch.transitions += 1;
    let outcome = match task.spec {
        ActionSpec::TurnSetup => unreachable!(),
        ActionSpec::PlayCard {
            card_handle,
            target_handle,
            ..
        } => CombatEngine::play_card_prechecked(
            &mut scratch.state,
            content,
            card_handle,
            target_handle,
            &mut empty_q,
        ),
        ActionSpec::UsePotion {
            potion_slot,
            target_handle,
            ..
        } => CombatEngine::use_potion(
            &mut scratch.state,
            content,
            potion_slot,
            target_handle,
            &mut empty_q,
        ),
        ActionSpec::EndTurn => CombatEngine::end_player_turn(&mut scratch.state, content),
    };

    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(err) => {
            if should_abort_search_on_kernel_error(&err) {
                scratch.record_fatal_error(err);
            }
            return false;
        }
    };

    match outcome {
        StepOutcome::Completed | StepOutcome::CombatEnded { .. } => {
            finalize_candidate(
                scratch,
                task,
                action,
                por_key,
                false,
                &[],
                0,
                content,
                root_baseline,
                policy,
                profile,
                cert,
                frozen_incumbent,
                tt,
                sensitivity,
                allow_paid_optional_potions,
                eval_mode,
            );
            false
        }
        StepOutcome::NeedsChoice(req) => {
            if !allows_brightest_flame(
                policy.brightest_flame_max_hp_loss_limit,
                root_baseline.initial_brightest_flame_max_hp_spent,
                root_baseline.brightest_flame_spent(&scratch.state),
            ) {
                return false;
            }
            let first_pile = req.source_pile;
            let is_end_turn = matches!(task.spec, ActionSpec::EndTurn);
            let (is_delayed_or_end_turn, max_branches, resolved_cap) =
                choice_probe_limits(profile, first_pile, is_end_turn, false);
            let max_choice_depth = 8;
            scratch.begin_choice_tree();
            clone_battle_state_into(&scratch.state, &mut scratch.choice_paused_states[0]);
            let mut choices_so_far: SmallVec<[SmallVec<[usize; 8]>; 2]> = SmallVec::new();
            resolve_choices_recursive(
                scratch,
                &mut choices_so_far,
                0,
                task,
                action,
                por_key,
                is_delayed_or_end_turn,
                false,
                max_branches,
                resolved_cap,
                max_choice_depth,
                defer_large_choice_probes,
                static_ctx,
                content,
                root_baseline,
                policy,
                profile,
                cert,
                frozen_incumbent,
                tt,
                probe_cache,
                sensitivity,
                allow_paid_optional_potions,
                eval_mode,
            );
            if scratch.fatal_error.is_some() {
                return false;
            }
            commit_resolved_choices(
                scratch,
                task,
                task_idx,
                defer_large_choice_probes,
                turn_boundary_rollouts_enabled,
                first_pile,
                is_end_turn,
                false,
                max_branches,
                static_ctx,
                content,
                root_baseline,
                profile,
                eval_mode,
                probe_cache,
            )
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn expand_wave_tasks(
    parent_slice: &[NodeId],
    dop: usize,
    pool: Option<&rayon::ThreadPool>,
    static_ctx: &Arc<RootStaticContext>,
    arena: &NodeArena,
    parent_source: ParentStateSource<'_>,
    content: &(dyn ContentDispatcher + Sync),
    root_baseline: &RootEvalBaseline,
    policy: &SearchPolicy,
    profile: &SolverSearchProfile,
    cert: &ClosureCertificate,
    frozen_incumbent: &FrozenIncumbentBound,
    tt: &ShardedParetoTT,
    probe_cache: &mut ChoiceProbeCache,
    sensitivity: &CombatOrderSensitivity,
    allow_paid_optional_potions: bool,
    turn_boundary_rollouts_enabled: bool,
    eval_mode: EvalMode,
    cancel: &AtomicBool,
    abort: &SearchAbortSlot,
    scratches: &mut [WorkerScratch],
    parent_states: &mut Vec<BattleState>,
    enumerated_tasks: &mut Vec<Vec<ActionTask>>,
    flat_tasks: &mut Vec<(usize, ActionTask)>,
    deferred_batches: &mut Vec<DeferredChoiceBatch>,
    probe_jobs: &mut Vec<ProbeJob>,
    parent_slices: &mut Vec<AtomicU64>,
) -> usize {
    for s in scratches.iter_mut() {
        if s.wave_dirty {
            s.reset_for_wave();
        }
    }
    if parent_slice.is_empty() {
        return 0;
    }

    if dop <= 1 || pool.is_none() {
        if parent_slices.len() < parent_slice.len() {
            parent_slices.resize_with(parent_slice.len(), || AtomicU64::new(0));
        }
        scratches[0].wave_dirty = true;
        let scratch = &mut scratches[0];
        let parent_state = &mut parent_states[0];
        let mut probe_access = ProbeCacheAccess::SingleThread(probe_cache);
        for (parent_rank, &pid) in parent_slice.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) || abort.is_tripped() {
                parent_slices[parent_rank].store(0, Ordering::Relaxed);
                continue;
            }
            let pnode = arena.get(pid);
            let p_has_la = pnode.has_choice_lookahead();
            let p_la_score = pnode.choice_lookahead_score();
            let pbytes = parent_source.get(pnode.state_handle);
            // SAFETY: `pbytes` was committed from `pack_state` output in this process.
            if unsafe { unpack_state(pbytes, static_ctx, parent_state) }.is_err() {
                parent_slices[parent_rank].store(0, Ordering::Relaxed);
                continue;
            }
            let mut local_tasks = std::mem::take(&mut scratch.action_buf);
            local_tasks.clear();
            enumerate_parent_tasks(
                parent_rank as u32,
                pid,
                pnode,
                p_has_la,
                p_la_score,
                parent_state,
                content,
                policy,
                profile,
                sensitivity,
                root_baseline,
                allow_paid_optional_potions,
                &mut local_tasks,
                &mut scratch.por_pruned,
                &mut scratch.branch_truncations,
            );
            let c_start = scratch.candidates.len();
            for task in &local_tasks {
                if cancel.load(Ordering::Relaxed) || abort.is_tripped() {
                    break;
                }
                let _ = execute_action_from_parent(
                    scratch,
                    parent_state,
                    task,
                    0,
                    false,
                    turn_boundary_rollouts_enabled,
                    static_ctx,
                    content,
                    root_baseline,
                    policy,
                    profile,
                    cert,
                    frozen_incumbent,
                    tt,
                    &mut probe_access,
                    sensitivity,
                    allow_paid_optional_potions,
                    eval_mode,
                );
                if let Some(err) = scratch.fatal_error.take() {
                    abort.record_error(err);
                    break;
                }
            }
            let c_len = scratch.candidates.len() - c_start;
            parent_slices[parent_rank].store(
                if c_len > 0 {
                    pack_parent_slice(0, c_start, c_len)
                } else {
                    0
                },
                Ordering::Relaxed,
            );
            scratch.action_buf = local_tasks;
        }
        return parent_slice.len();
    }

    let pool = pool.unwrap();
    debug_assert!(scratches.len() >= dop);
    const PARENT_UNPACK_BATCH: usize = 512;
    let total_parents = parent_slice.len();
    let mut global_task_offset = 0usize;

    for chunk_start in (0..total_parents).step_by(PARENT_UNPACK_BATCH) {
        if cancel.load(Ordering::Relaxed) || abort.is_tripped() {
            break;
        }
        let chunk_end = (chunk_start + PARENT_UNPACK_BATCH).min(total_parents);
        let batch_len = chunk_end - chunk_start;
        while parent_states.len() < batch_len {
            parent_states.push(empty_scratch_state(static_ctx.clone()));
        }
        while enumerated_tasks.len() < batch_len {
            enumerated_tasks.push(Vec::new());
        }

        let (por_sum, trunc_sum) = pool.install(|| {
            parent_states[..batch_len]
                .par_iter_mut()
                .zip(enumerated_tasks[..batch_len].par_iter_mut())
                .enumerate()
                .map(|(rel_p, (pstate, tasks_vec))| {
                    tasks_vec.clear();
                    if cancel.load(Ordering::Relaxed) || abort.is_tripped() {
                        return (0u64, 0u64);
                    }
                    let parent_rank = chunk_start + rel_p;
                    let pid = parent_slice[parent_rank];
                    let pnode = arena.get(pid);
                    let p_has_la = pnode.has_choice_lookahead();
                    let p_la_score = pnode.choice_lookahead_score();
                    let pbytes = parent_source.get(pnode.state_handle);
                    // SAFETY: `pbytes` was committed from `pack_state` output in this process.
                    if unsafe { unpack_state(pbytes, static_ctx, pstate) }.is_err() {
                        return (0u64, 0u64);
                    }
                    let mut local_por = 0u64;
                    let mut local_trunc = 0u64;
                    enumerate_parent_tasks(
                        parent_rank as u32,
                        pid,
                        pnode,
                        p_has_la,
                        p_la_score,
                        pstate,
                        content,
                        policy,
                        profile,
                        sensitivity,
                        root_baseline,
                        allow_paid_optional_potions,
                        tasks_vec,
                        &mut local_por,
                        &mut local_trunc,
                    );
                    (local_por, local_trunc)
                })
                .reduce(|| (0u64, 0u64), |a, b| (a.0 + b.0, a.1 + b.1))
        });
        scratches[0].wave_dirty = true;
        scratches[0].por_pruned += por_sum;
        scratches[0].branch_truncations += trunc_sum;

        flat_tasks.clear();
        for rel_p in 0..batch_len {
            let tasks_vec = &mut enumerated_tasks[rel_p];
            for t in tasks_vec.drain(..) {
                flat_tasks.push((rel_p, t));
            }
        }
        let chunk_tasks = flat_tasks.len();
        if chunk_tasks == 0 {
            continue;
        }
        let new_total = global_task_offset + chunk_tasks;
        if parent_slices.len() < new_total {
            parent_slices.resize_with(new_total, || AtomicU64::new(0));
        }
        for slot in &parent_slices[global_task_offset..new_total] {
            slot.store(0, Ordering::Relaxed);
        }

        if chunk_tasks == 1 || scratches.len() < 2 {
            let mut probe_access = ProbeCacheAccess::Shared(probe_cache);
            for (rel_t, &(rel_p, ref task)) in flat_tasks.iter().enumerate() {
                if cancel.load(Ordering::Relaxed) || abort.is_tripped() {
                    break;
                }
                let task_idx = global_task_offset + rel_t;
                let scratch = &mut scratches[0];
                scratch.wave_dirty = true;
                let c_start = scratch.candidates.len();
                let deferred = execute_action_from_parent(
                    scratch,
                    &parent_states[rel_p],
                    task,
                    task_idx,
                    true,
                    turn_boundary_rollouts_enabled,
                    static_ctx,
                    content,
                    root_baseline,
                    policy,
                    profile,
                    cert,
                    frozen_incumbent,
                    tt,
                    &mut probe_access,
                    sensitivity,
                    allow_paid_optional_potions,
                    eval_mode,
                );
                if let Some(err) = scratch.fatal_error.take() {
                    abort.record_error(err);
                    break;
                } else if !deferred {
                    let c_len = scratch.candidates.len() - c_start;
                    if c_len > 0 {
                        parent_slices[task_idx]
                            .store(pack_parent_slice(0, c_start, c_len), Ordering::Relaxed);
                    }
                }
            }
        } else {
            let active = dop.min(chunk_tasks).min(scratches.len());
            let next_task = AtomicUsize::new(0);
            let tasks_ref: &[(usize, ActionTask)] = flat_tasks.as_slice();
            let parents_ref = &parent_states[..batch_len];
            let slices_ref = &parent_slices[..new_total];
            let probe_cache_ref: &ChoiceProbeCache = &*probe_cache;
            let (first_scratch, rest_scratches) =
                scratches[..active].split_first_mut().expect("active >= 2");
            let run_task_worker = |w_idx: usize, scratch: &mut WorkerScratch| {
                scratch.wave_dirty = true;
                let mut probe_access = ProbeCacheAccess::Shared(probe_cache_ref);
                loop {
                    if cancel.load(Ordering::Relaxed) || abort.is_tripped() {
                        break;
                    }
                    let rel_t = next_task.fetch_add(1, Ordering::Relaxed);
                    if rel_t >= chunk_tasks {
                        break;
                    }
                    let task_idx = global_task_offset + rel_t;
                    let (rel_p, ref task) = tasks_ref[rel_t];
                    let c_start = scratch.candidates.len();
                    let deferred = execute_action_from_parent(
                        scratch,
                        &parents_ref[rel_p],
                        task,
                        task_idx,
                        true,
                        turn_boundary_rollouts_enabled,
                        static_ctx,
                        content,
                        root_baseline,
                        policy,
                        profile,
                        cert,
                        frozen_incumbent,
                        tt,
                        &mut probe_access,
                        sensitivity,
                        allow_paid_optional_potions,
                        eval_mode,
                    );
                    if let Some(err) = scratch.fatal_error.take() {
                        abort.record_error(err);
                        break;
                    }
                    if !deferred {
                        let c_len = scratch.candidates.len() - c_start;
                        if c_len > 0 {
                            slices_ref[task_idx].store(
                                pack_parent_slice(w_idx, c_start, c_len),
                                Ordering::Relaxed,
                            );
                        }
                    }
                }
            };
            let worker_fn = &run_task_worker;
            pool.scope(|scope| {
                for (i, scratch) in rest_scratches.iter_mut().enumerate() {
                    scope.spawn(move |_| worker_fn(i + 1, scratch));
                }
                worker_fn(0, first_scratch);
            });
        }
        global_task_offset = new_total;
    }

    if global_task_offset > 0 {
        resolve_deferred_choice_batches_on_pool(
            dop,
            Some(pool),
            scratches,
            deferred_batches,
            probe_jobs,
            probe_cache,
            static_ctx,
            content,
            root_baseline,
            profile,
            eval_mode,
            cancel,
            &parent_slices[..global_task_offset],
            true,
        );
    }
    global_task_offset
}

fn replay_fixed_prefix(
    root: &BattleState,
    prefix: &[PlannedAction],
    policy: &SearchPolicy,
    root_baseline: &RootEvalBaseline,
    content: &dyn ContentDispatcher,
) -> Result<(BattleState, u16, u16, i32, i32, u16, u16, u64, u16), KernelError> {
    let mut state = root.clone();
    let mut potion_count = 0u16;
    let mut optional_paid_count = 0u16;
    let mut potion_cost = 0i32;
    let mut optional_paid_cost = 0i32;
    let mut ambergris_count = 0u16;
    let mut used_potion_slot_mask = 0u16;
    let mut forced_used_mask = 0u64;
    let mut forced_ambergris_count = 0u16;
    for act in prefix {
        match act {
            PlannedAction::TurnSetup { choices } => {
                let q = choices.clone().unwrap_or_default();
                for sel in q {
                    let _ = CombatEngine::resume_choice(&mut state, content, &sel)?;
                }
            }
            PlannedAction::PlayCard {
                card_handle,
                target_handle,
                choices,
            } => {
                let mut q: SmallVec<[Vec<usize>; 2]> =
                    choices.clone().unwrap_or_default().into_iter().collect();
                let mut out = CombatEngine::play_card(
                    &mut state,
                    content,
                    CardHandle(*card_handle as u32),
                    target_handle.map(|h| CreatureHandle(h as u32)),
                    &mut q,
                )?;
                while matches!(out, StepOutcome::NeedsChoice(_)) && !q.is_empty() {
                    let sel = q.remove(0);
                    out = CombatEngine::resume_choice(&mut state, content, &sel)?;
                }
            }
            PlannedAction::UsePotion {
                potion_slot,
                potion_id,
                target_handle,
                choices,
            } => {
                let slot_bit = if *potion_slot < 16 {
                    1u16 << *potion_slot
                } else {
                    0
                };
                let is_generated = (state.player.combat_generated_potion_mask & slot_bit) != 0;
                potion_count += 1;
                if !is_generated {
                    let cost = if *potion_slot < 16 && root_baseline.root_slot_costs[*potion_slot] >= 0
                    {
                        root_baseline.root_slot_costs[*potion_slot] as i32
                    } else {
                        PotionValuationRegistry::strategic_hp_cost_for_root(
                            *potion_id,
                            root_baseline.renewable_potion_shaped_rock,
                            root_baseline.initial_player_max_hp,
                            &policy.potion_hp_threshold_overrides,
                        )
                    };
                    let amb = u16::from(*potion_id == PotionId::Ambergris);
                    potion_cost += cost;
                    ambergris_count += amb;
                    used_potion_slot_mask |= slot_bit;
                    if let Some(bit_idx) = policy.forced_bit_for(*potion_slot, *potion_id) {
                        let bit = if bit_idx < 64 { 1u64 << bit_idx } else { 0 };
                        if (forced_used_mask & bit) == 0 {
                            forced_used_mask |= bit;
                            forced_ambergris_count += amb;
                        }
                    } else if cost > 0 {
                        optional_paid_count += 1;
                        optional_paid_cost += cost;
                    }
                }
                let mut q: SmallVec<[Vec<usize>; 2]> =
                    choices.clone().unwrap_or_default().into_iter().collect();
                let mut out = CombatEngine::use_potion(
                    &mut state,
                    content,
                    *potion_slot,
                    target_handle.map(|h| CreatureHandle(h as u32)),
                    &mut q,
                )?;
                while matches!(out, StepOutcome::NeedsChoice(_)) && !q.is_empty() {
                    let sel = q.remove(0);
                    out = CombatEngine::resume_choice(&mut state, content, &sel)?;
                }
            }
            PlannedAction::EndTurn { choices } => {
                let mut q: SmallVec<[Vec<usize>; 2]> =
                    choices.clone().unwrap_or_default().into_iter().collect();
                let mut out = CombatEngine::end_player_turn(&mut state, content)?;
                while matches!(out, StepOutcome::NeedsChoice(_)) && !q.is_empty() {
                    let sel = q.remove(0);
                    out = CombatEngine::resume_choice(&mut state, content, &sel)?;
                }
            }
        }
    }
    Ok((
        state,
        potion_count,
        optional_paid_count,
        potion_cost,
        optional_paid_cost,
        ambergris_count,
        used_potion_slot_mask,
        forced_used_mask,
        forced_ambergris_count,
    ))
}

fn make_stand_pat_task(
    parent_rank: u32,
    parent_id: NodeId,
    pnode: &SearchNode,
    parent_filled_potions: u8,
    action_ord: u32,
) -> ActionTask {
    ActionTask {
        parent_rank,
        parent_id,
        parent_turn: pnode.turn as i32,
        parent_action_count: pnode.action_count,
        parent_potion_count: pnode.potion_count as u16,
        parent_explicit_potion_count: pnode.explicit_potion_count as u16,
        parent_optional_paid_potion_count: pnode.optional_paid_potion_count as u16,
        parent_automatic_potion_count: pnode.automatic_potion_count as u16,
        parent_filled_potions,
        parent_used_potion_slot_mask: pnode.used_potion_slot_mask,
        parent_potion_cost: pnode.potion_strategic_cost as i32,
        parent_optional_paid_potion_cost: pnode.optional_paid_potion_cost as i32,
        parent_ambergris_count: pnode.ambergris_count as u16,
        parent_forced_used_mask: pnode.forced_used_mask,
        parent_forced_ambergris_count: pnode.forced_ambergris_count as u16,
        parent_has_choice_lookahead: false,
        parent_choice_lookahead_score: None,
        action_ord,
        spec: ActionSpec::EndTurn,
    }
}

fn select_stand_pat_candidates(
    active_frontier: &[NodeId],
    arena: &NodeArena,
    max_total: usize,
    prefer_potion_buckets: bool,
) -> SmallVec<[NodeId; 16]> {
    let cap = max_total.min(active_frontier.len()).min(16);
    let mut out = SmallVec::new();
    if cap == 0 {
        return out;
    }

    let mut counts = [0usize; 4];
    for &id in active_frontier {
        let b = (arena.get(id).potion_count as usize).min(3);
        counts[b] += 1;
    }
    let active_buckets = counts.iter().filter(|&&c| c > 0).count();
    if active_buckets <= 1 {
        for &id in active_frontier.iter().take(cap) {
            out.push(id);
        }
        return out;
    }

    let mut quotas = [0usize; 4];
    let mut remaining = cap;
    let start_bucket = usize::from(prefer_potion_buckets);
    while remaining > 0 {
        let mut progressed = false;
        for step in 0..4 {
            if remaining == 0 {
                break;
            }
            let b = (start_bucket + step) % 4;
            if quotas[b] < counts[b] {
                quotas[b] += 1;
                remaining -= 1;
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }

    let mut taken = [0usize; 4];
    for &id in active_frontier {
        if out.len() >= cap {
            break;
        }
        let b = (arena.get(id).potion_count as usize).min(3);
        if taken[b] >= quotas[b] {
            continue;
        }
        taken[b] += 1;
        out.push(id);
    }
    out
}

#[inline(always)]
fn projected_battle_hp_lost_from_cumulative(
    root: &RootEvalBaseline,
    cumulative_hp_lost: i32,
) -> i32 {
    root.prior_battle_hp_lost + (cumulative_hp_lost - root.initial_cumulative_hp_lost).max(0)
}

#[inline(always)]
fn are_strategy_targets_satisfied_raw(
    root: &RootEvalBaseline,
    fast: &StoredFastEval,
    rare: &RareNodeEval,
) -> bool {
    if fast.all_strategy_targets_satisfied() {
        return true;
    }
    let growth_ok = !root.effective_has_growth_targets
        || root
            .growth_opportunity_targets
            .is_satisfied_by(&rare.growth_rewards);
    let relic_ok = rare.relic_eval.all_satisfied();
    growth_ok && relic_ok
}

#[inline(always)]
fn are_strategy_targets_satisfied_output(root: &RootEvalBaseline, out: &BeamRunOutput) -> bool {
    if out.all_strategy_targets_satisfied {
        return true;
    }
    let growth_ok = !root.effective_has_growth_targets
        || root
            .growth_opportunity_targets
            .is_satisfied_by(&out.growth_rewards);
    let relic_ok = out.relic_eval.all_satisfied();
    growth_ok && relic_ok
}

#[derive(Clone, Copy, Debug)]
struct BestVictoryState {
    win_turn: i32,
    raw_won_hp_lost: i32,
    projected_battle_hp_lost: i32,
    total_potions: u16,
    optional_paid_potion_count: u16,
    net_hp_delta_centi: i32,
    net_potion_cost_centi: i32,
    strategic_hp_deficit: i32,
    strategy_goal_hp_credit: i32,
    strategy_goal_count: i32,
    death_save_use_count: i32,
    outstanding_stolen: i32,
    forfeits_gold: bool,
    all_strategy_targets_satisfied: bool,
    enemy_hp: i32,
    score: i64,
}

#[inline]
fn is_better_victory(
    cand: &BestVictoryState,
    cur: &BestVictoryState,
    check_theft: bool,
) -> bool {
    if cand.death_save_use_count != cur.death_save_use_count {
        return cand.death_save_use_count < cur.death_save_use_count;
    }
    if check_theft {
        if cand.outstanding_stolen != cur.outstanding_stolen {
            return cand.outstanding_stolen < cur.outstanding_stolen;
        }
        if cand.forfeits_gold != cur.forfeits_gold {
            return !cand.forfeits_gold;
        }
    }
    let cand_v = (cand.net_hp_delta_centi as i64) - (cand.net_potion_cost_centi as i64);
    let cur_v = (cur.net_hp_delta_centi as i64) - (cur.net_potion_cost_centi as i64);
    if cand_v != cur_v {
        return cand_v > cur_v;
    }
    if cand.net_hp_delta_centi != cur.net_hp_delta_centi {
        return cand.net_hp_delta_centi > cur.net_hp_delta_centi;
    }
    if cand.strategy_goal_hp_credit != cur.strategy_goal_hp_credit {
        return cand.strategy_goal_hp_credit > cur.strategy_goal_hp_credit;
    }
    if cand.strategy_goal_count != cur.strategy_goal_count {
        return cand.strategy_goal_count > cur.strategy_goal_count;
    }
    if cand.projected_battle_hp_lost != cur.projected_battle_hp_lost {
        return cand.projected_battle_hp_lost < cur.projected_battle_hp_lost;
    }
    if cand.total_potions != cur.total_potions {
        return cand.total_potions < cur.total_potions;
    }
    if cand.win_turn != cur.win_turn {
        return cand.win_turn < cur.win_turn;
    }
    if cand.enemy_hp != cur.enemy_hp {
        return cand.enemy_hp < cur.enemy_hp;
    }
    cand.score > cur.score
}

#[inline]
fn keep_early_beam_width(
    best_victory: Option<&BestVictoryState>,
    root_baseline: &RootEvalBaseline,
) -> bool {
    let Some(v) = best_victory else {
        return true;
    };
    let has_any_strategy_targets = root_baseline.effective_has_growth_targets
        || !root_baseline.active_relic_targets.is_empty();
    v.projected_battle_hp_lost > 0
        || v.strategic_hp_deficit > 0
        || v.optional_paid_potion_count > 0
        || v.death_save_use_count > 0
        || (has_any_strategy_targets && !v.all_strategy_targets_satisfied)
}

#[inline]
fn has_exceeded_post_victory_turn_cap(
    best_victory: Option<&BestVictoryState>,
    next_turn_number: i32,
    root_baseline: &RootEvalBaseline,
    policy: &SearchPolicy,
    is_elite_or_boss: bool,
    allow_paid_optional_potions: bool,
) -> bool {
    let Some(v) = best_victory else {
        return false;
    };
    if v.win_turn == i32::MAX {
        return false;
    }
    let has_any_strategy_targets = root_baseline.effective_has_growth_targets
        || !root_baseline.active_relic_targets.is_empty();
    let has_relic_targets = !root_baseline.active_relic_targets.is_empty();
    let unsatisfied_strategy_targets =
        has_any_strategy_targets && !v.all_strategy_targets_satisfied;

    let check_theft = policy.theft_policy == SolverTheftPolicy::PreserveResources;
    let can_heal_or_grow = root_baseline.has_any_heal_or_unknown_generation
        || unsatisfied_strategy_targets
        || (allow_paid_optional_potions
            && (root_baseline.paid_heal_potion_slot_mask != 0
                || root_baseline.paid_fruit_juice_slot_mask != 0
                || root_baseline.paid_entropic_brew_slot_mask != 0))
        || v.death_save_use_count > 0
        || (check_theft && (v.outstanding_stolen > 0 || v.forfeits_gold));

    let took_persistent_damage = v.net_hp_delta_centi < 0 || v.strategic_hp_deficit > 0;
    let took_raw_battle_hp_loss = v.projected_battle_hp_lost > 0;
    let spent_optional_paid_potion =
        v.net_potion_cost_centi > 0 || v.optional_paid_potion_count > 0;

    let max_post_victory_turn = if unsatisfied_strategy_targets {
        let strategy_margin = if has_relic_targets { 5 } else { 4 };
        let strategy_floor = if has_relic_targets { 10 } else { 9 };
        (v.win_turn + strategy_margin).max(strategy_floor)
    } else if can_heal_or_grow || took_persistent_damage || spent_optional_paid_potion {
        (v.win_turn + 4).max(8)
    } else {
        let post_victory_margin = if took_raw_battle_hp_loss {
            if is_elite_or_boss {
                3
            } else {
                2
            }
        } else {
            1
        };
        v.win_turn + post_victory_margin
    };

    next_turn_number > max_post_victory_turn
}

#[allow(clippy::too_many_arguments)]
fn record_victory_and_tighten_precomputed(
    root_baseline: &RootEvalBaseline,
    incumbent_table: &PrimaryIncumbentTable,
    incumbent: &mut FrozenIncumbentBound,
    policy: &SearchPolicy,
    fast_eval: &StoredFastEval,
    rare: &RareNodeEval,
    win_turn: i32,
    total_potions: u16,
    explicit_potion_count: u16,
    optional_paid_potion_count: u16,
    potion_strategic_cost: i32,
    forced_used_mask: u64,
    net_hp_delta_centi: i32,
    net_potion_cost_centi: i32,
    deficit: i32,
    score: i64,
    has_victory: &mut bool,
    best_victory: &mut Option<BestVictoryState>,
) {
    incumbent_table.tighten(
        PrimaryIncumbentBucket {
            potions: total_potions,
            forfeits_gold: fast_eval.forfeits_gold(),
        },
        PrimarySearchIncumbent {
            strategic_hp_deficit: deficit,
            combat_ended_turn: win_turn,
            potion_strategic_cost,
        },
    );

    let enforce_forced = root_baseline.all_forced_potion_mask != 0;
    let forced_ok = !enforce_forced
        || (forced_used_mask & root_baseline.all_forced_potion_mask)
            == root_baseline.all_forced_potion_mask;
    let req_ok = (explicit_potion_count as usize) >= root_baseline.minimum_required_potion_uses;
    let disabled_ok = root_baseline.effective_potion_policy != SolverPotionPolicy::Disabled
        || (optional_paid_potion_count == 0
            && (explicit_potion_count as usize) <= root_baseline.forced_directive_count);
    if !forced_ok || !req_ok || !disabled_ok {
        return;
    }

    let check_theft = policy.theft_policy == SolverTheftPolicy::PreserveResources;
    incumbent.tighten(
        root_baseline,
        true,
        fast_eval.death_save_use_count as i32,
        net_hp_delta_centi,
        net_potion_cost_centi,
        fast_eval.cumulative_hp_lost as i32,
        total_potions as usize,
        optional_paid_potion_count as usize,
        explicit_potion_count as usize,
        win_turn,
        fast_eval.outstanding_stolen as i32,
        fast_eval.forfeits_gold(),
        check_theft,
    );

    let raw_won_hp_lost = fast_eval.cumulative_hp_lost as i32;
    let cand_v = BestVictoryState {
        win_turn,
        raw_won_hp_lost,
        projected_battle_hp_lost: projected_battle_hp_lost_from_cumulative(
            root_baseline,
            raw_won_hp_lost,
        ),
        total_potions,
        optional_paid_potion_count,
        net_hp_delta_centi,
        net_potion_cost_centi,
        strategic_hp_deficit: deficit,
        strategy_goal_hp_credit: fast_eval.strategy_goal_hp_credit as i32,
        strategy_goal_count: fast_eval.strategy_goal_count as i32,
        death_save_use_count: fast_eval.death_save_use_count as i32,
        outstanding_stolen: fast_eval.outstanding_stolen as i32,
        forfeits_gold: fast_eval.forfeits_gold(),
        all_strategy_targets_satisfied: are_strategy_targets_satisfied_raw(
            root_baseline,
            fast_eval,
            rare,
        ),
        enemy_hp: fast_eval.enemy_hp,
        score,
    };
    *has_victory = true;
    if best_victory
        .as_ref()
        .is_none_or(|cur| is_better_victory(&cand_v, cur, check_theft))
    {
        *best_victory = Some(cand_v);
    }
}

#[allow(clippy::too_many_arguments)]
fn record_victory_and_tighten(
    root_baseline: &RootEvalBaseline,
    incumbent_table: &PrimaryIncumbentTable,
    incumbent: &mut FrozenIncumbentBound,
    policy: &SearchPolicy,
    fast_eval: &StoredFastEval,
    rare: &RareNodeEval,
    win_turn: i32,
    total_potions: u16,
    explicit_potion_count: u16,
    optional_paid_potion_count: u16,
    potion_strategic_cost: i32,
    optional_paid_potion_cost: i32,
    forced_used_mask: u64,
    score: i64,
    has_victory: &mut bool,
    best_victory: &mut Option<BestVictoryState>,
) {
    let end_max_hp = if fast_eval.player_max_hp > 0 {
        fast_eval.player_max_hp as i32
    } else {
        root_baseline.initial_player_max_hp.max(1)
    };
    let relic_heal = ActEndingBossPolicy::ranked_post_combat_relic_heal(
        root_baseline.post_combat_relic_heal,
        true,
        fast_eval.player_hp as i32,
        end_max_hp,
    );
    let net_hp_delta_centi = ActEndingBossPolicy::compute_net_persistent_hp_delta_centi(
        fast_eval.player_hp as i32,
        relic_heal,
        root_baseline.initial_player_hp,
        end_max_hp,
        root_baseline.initial_player_max_hp,
        fast_eval.death_save_hp_restored as i32,
        fast_eval.strategic_hp_credit as i32,
        root_baseline.boss_hp_relief,
        root_baseline.ignore_long_term_rewards,
        i32::from(fast_eval.feed_count) * root_baseline.feed_max_hp_per_kill,
    );
    let net_potion_cost_centi = PotionUsePolicy::compute_net_potion_opportunity_cost_centi(
        optional_paid_potion_cost,
        optional_paid_potion_count as i32,
        fast_eval.alchemize_count as i32,
        fast_eval.empty_potion_slots as i32,
        root_baseline,
    );
    let deficit = ActEndingBossPolicy::round_centi_to_hp(-net_hp_delta_centi);
    record_victory_and_tighten_precomputed(
        root_baseline,
        incumbent_table,
        incumbent,
        policy,
        fast_eval,
        rare,
        win_turn,
        total_potions,
        explicit_potion_count,
        optional_paid_potion_count,
        potion_strategic_cost,
        forced_used_mask,
        net_hp_delta_centi,
        net_potion_cost_centi,
        deficit,
        score,
        has_victory,
        best_victory,
    );
}

#[inline]
fn push_worker_candidate_node(
    arena: &mut NodeArena,
    cand: &WorkerCandidate,
    state_handle: PackedHandle,
    choices_offset: u32,
    choices_len: u8,
) -> NodeId {
    let mut node_flags = 0u8;
    if cand.has_extra_end_turn {
        node_flags |= NODE_FLAG_HAS_EXTRA_END_TURN;
    }
    if cand.is_terminal {
        node_flags |= NODE_FLAG_IS_TERMINAL;
    }
    let rare_eval_idx = arena.push_opt_rare_eval(cand.rare_eval);
    let (
        beam_rank_score,
        rollout_score,
        effective_defense,
        board_investment,
        persistent_buff_value,
        max_dark_evoke,
        total_poison_and_doom,
        setup_score,
        reachable_hand_value,
        optionality_cards,
        best_power_priority,
        tactical_sig,
    ) = if let Some(ref full) = cand.full_eval {
        node_flags |= NODE_FLAG_HAS_FULL_EVAL;
        let rs = if let Some(rs) = full.rollout_score {
            node_flags |= NODE_FLAG_HAS_ROLLOUT_SCORE;
            rs
        } else {
            full.choice_lookahead_score.unwrap_or(0.0)
        };
        if full.has_choice_lookahead {
            node_flags |= NODE_FLAG_HAS_CHOICE_LOOKAHEAD;
        }
        (
            full.beam_rank_score,
            rs,
            full.effective_defense.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            full.board_investment.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            full.persistent_buff_value
                .clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            full.max_dark_evoke.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            full.total_poison_and_doom
                .clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            full.setup_score.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            full.reachable_hand_value
                .clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            full.optionality_cards.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            full.best_power_priority,
            full.tactical_sig,
        )
    } else {
        (
            cand.base_score,
            0.0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            PowerRoutePriority::Low,
            0,
        )
    };
    let turn = cand.turn.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
    let potion_strategic_cost =
        cand.potion_strategic_cost.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
    let optional_paid_potion_cost = cand
        .optional_paid_potion_cost
        .clamp(i16::MIN as i32, i16::MAX as i32)
        as i16;
    let potion_count = cand.potion_count.min(u8::MAX as u16) as u8;
    let explicit_potion_count = cand.explicit_potion_count.min(u8::MAX as u16) as u8;
    let optional_paid_potion_count =
        cand.optional_paid_potion_count.min(u8::MAX as u16) as u8;
    let automatic_potion_count = cand.automatic_potion_count.min(u8::MAX as u16) as u8;
    let ambergris_count = cand.ambergris_count.min(u8::MAX as u16) as u8;
    let forced_ambergris_count = cand.forced_ambergris_count.min(u8::MAX as u16) as u8;
    arena.push_node_with(|| SearchNode {
        state_hash: cand.state_hash,
        forced_used_mask: cand.forced_used_mask,
        tactical_sig,
        beam_rank_score,
        rollout_score,
        por_key: cand.por_key,
        fast_eval: cand.fast_eval,
        state_handle,
        action: cand.action,
        parent_raw: cand.parent_id.0,
        choices_offset,
        rare_eval_idx,
        effective_defense,
        board_investment,
        persistent_buff_value,
        max_dark_evoke,
        total_poison_and_doom,
        turn,
        action_count: cand.action_count,
        used_potion_slot_mask: cand.used_potion_slot_mask,
        potion_strategic_cost,
        optional_paid_potion_cost,
        setup_score,
        reachable_hand_value,
        optionality_cards,
        choices_len,
        potion_count,
        explicit_potion_count,
        optional_paid_potion_count,
        automatic_potion_count,
        ambergris_count,
        forced_ambergris_count,
        boundary_rank: 0,
        best_power_priority,
        node_flags,
    })
}

#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn commit_terminal_candidate(
    cand: &WorkerCandidate,
    staged_choices: &ChoiceArena,
    root_baseline: &RootEvalBaseline,
    policy: &SearchPolicy,
    incumbent_table: &PrimaryIncumbentTable,
    incumbent: &mut FrozenIncumbentBound,
    check_theft: bool,
    options: &PassSearchOptions,
    arena: &mut NodeArena,
    completed: &mut Vec<NodeId>,
    terminal_tracker: &mut crate::eval::FinalPlanTracker,
    has_victory: &mut bool,
    best_victory: &mut Option<BestVictoryState>,
    hp_target_reached: &mut bool,
) {
    let turn = cand.turn.clamp(i16::MIN as i32, i16::MAX as i32);
    let optional_paid_potion_cost = cand
        .optional_paid_potion_cost
        .clamp(i16::MIN as i32, i16::MAX as i32)
        as i16;
    let potion_count = cand.potion_count.min(u8::MAX as u16) as u8;
    let explicit_potion_count = cand.explicit_potion_count.min(u8::MAX as u16) as u8;
    let optional_paid_potion_count = cand.optional_paid_potion_count.min(u8::MAX as u16) as u8;
    let ambergris_count = cand.ambergris_count.min(u8::MAX as u16) as u8;
    let forced_ambergris_count = cand.forced_ambergris_count.min(u8::MAX as u16) as u8;
    if !cand.fast_eval.won()
        && terminal_tracker.can_fast_reject_non_victory(
            &cand.fast_eval,
            cand.action_count,
            cand.forced_used_mask,
            explicit_potion_count,
            optional_paid_potion_count,
            root_baseline,
        )
    {
        return;
    }
    let rare = cand.rare_eval.unwrap_or_default();
    let beam_rank_score = cand
        .full_eval
        .as_ref()
        .map_or(cand.base_score, |f| f.beam_rank_score);
    let (eval_cand, satisfies_hard) = terminal_tracker.evaluate_raw(
        NodeId(0),
        &cand.fast_eval,
        rare,
        cand.action_count,
        turn,
        potion_count,
        explicit_potion_count,
        optional_paid_potion_count,
        optional_paid_potion_cost,
        ambergris_count,
        cand.forced_used_mask,
        forced_ambergris_count,
        cand.used_potion_slot_mask,
        0,
        beam_rank_score,
        cand.state_hash,
        root_baseline,
    );
    if cand.fast_eval.won() {
        record_victory_and_tighten_precomputed(
            root_baseline,
            incumbent_table,
            incumbent,
            policy,
            &cand.fast_eval,
            &rare,
            cand.turn,
            cand.potion_count,
            cand.explicit_potion_count,
            cand.optional_paid_potion_count,
            cand.potion_strategic_cost,
            cand.forced_used_mask,
            eval_cand.net_persistent_hp_delta_centi,
            eval_cand.net_potion_opportunity_cost_centi,
            eval_cand.quality.strategic_hp_deficit,
            beam_rank_score.round() as i64,
            has_victory,
            best_victory,
        );
        if !*hp_target_reached
            && options.early_turn_scout_depth == 0
            && options.boss_tempo_scout_turns == 0
            && has_reached_acceptable_battle_hp_loss_raw(
                root_baseline,
                policy,
                &cand.fast_eval,
                &rare,
                cand.action_count,
                cand.forced_used_mask,
                optional_paid_potion_count,
                explicit_potion_count,
            )
            && (!options.allow_paid_optional_potions
                || !PotionUsePolicy::can_any_paid_potion_improve_on(
                    PaidPotionImproveInput {
                        won: true,
                        survives: true,
                        death_save_use_count: cand.fast_eval.death_save_use_count as i32,
                        end_player_hp: cand.fast_eval.player_hp as i32,
                        end_player_max_hp: cand.fast_eval.player_max_hp as i32,
                        used_potion_slot_mask: cand.used_potion_slot_mask,
                        optional_paid_potion_count: cand
                            .optional_paid_potion_count
                            .min(u8::MAX as u16) as u8,
                        raw_potion_strategic_cost: cand.optional_paid_potion_cost,
                        net_potion_opportunity_cost_centi: eval_cand
                            .net_potion_opportunity_cost_centi,
                        alchemize_count: cand.fast_eval.alchemize_count,
                        empty_potion_slots: cand.fast_eval.empty_potion_slots,
                        all_strategy_targets_satisfied: are_strategy_targets_satisfied_raw(
                            root_baseline,
                            &cand.fast_eval,
                            &rare,
                        ),
                        all_growth_targets_satisfied: !root_baseline.effective_has_growth_targets
                            || root_baseline
                                .growth_opportunity_targets
                                .is_satisfied_by(&rare.growth_rewards),
                        growth_hp_credit: cand.fast_eval.strategy_goal_hp_credit as i32,
                        relic_satisfied_mask: rare.relic_eval.satisfied_mask as u32,
                        outstanding_stolen: cand.fast_eval.outstanding_stolen as i32,
                        forfeits_gold: cand.fast_eval.forfeits_gold(),
                        theft_preserve_resources: check_theft,
                    },
                    root_baseline,
                ))
        {
            *hp_target_reached = true;
        }
    }
    if terminal_tracker.would_improve(&eval_cand, satisfies_hard) {
        let (choices_offset, choices_len) = arena.choices.extend_from_arena_slice(
            staged_choices,
            cand.choices_offset,
            cand.choices_len,
        );
        let nid = push_worker_candidate_node(
            arena,
            cand,
            PackedHandle::default(),
            choices_offset,
            choices_len,
        );
        terminal_tracker.record(nid, eval_cand, satisfies_hard);
        completed.push(nid);
    }
}

#[inline]
#[allow(clippy::too_many_arguments)]
fn commit_one_accepted(
    w_idx: usize,
    c_idx: usize,
    cand: &WorkerCandidate,
    slab: &LocalStateSlab,
    staged_choices: &ChoiceArena,
    root_baseline: &RootEvalBaseline,
    policy: &SearchPolicy,
    incumbent_table: &PrimaryIncumbentTable,
    incumbent: &mut FrozenIncumbentBound,
    check_theft: bool,
    options: &PassSearchOptions,
    arena: &mut NodeArena,
    turn_arena: &mut MainStateArena,
    completed: &mut Vec<NodeId>,
    terminal_tracker: &mut crate::eval::FinalPlanTracker,
    next_step_candidates: &mut Vec<NodeId>,
    next_turn_candidates: &mut Vec<NodeId>,
    defer_mid_turn: Option<&mut Vec<AcceptedMidTurn>>,
    packed_state_bytes_total: &mut u64,
    has_victory: &mut bool,
    best_victory: &mut Option<BestVictoryState>,
    hp_target_reached: &mut bool,
) {
    if cand.is_terminal {
        commit_terminal_candidate(
            cand,
            staged_choices,
            root_baseline,
            policy,
            incumbent_table,
            incumbent,
            check_theft,
            options,
            arena,
            completed,
            terminal_tracker,
            has_victory,
            best_victory,
            hp_target_reached,
        );
        return;
    }
    if !cand.turn_completed {
        if let Some(buf) = defer_mid_turn {
            let beam_score = cand
                .full_eval
                .as_ref()
                .map_or(cand.base_score, |f| f.beam_rank_score);
            buf.push(AcceptedMidTurn {
                w_idx: w_idx as u16,
                potion_count: cand.potion_count.min(u8::MAX as u16) as u8,
                explicit_potion_count: cand.explicit_potion_count.min(u8::MAX as u16) as u8,
                c_idx: c_idx as u32,
                beam_score,
            });
            return;
        }
    }
    let state_handle = if cand.turn_completed {
        let state_bytes = slab.get(cand.state_slab_idx, cand.state_off, cand.state_len);
        *packed_state_bytes_total += state_bytes.len() as u64;
        turn_arena.commit_slice(state_bytes)
    } else {
        PackedHandle {
            slab: w_idx as u32,
            offset: c_idx as u32,
            len: 0,
        }
    };
    let (choices_offset, choices_len) = arena.choices.extend_from_arena_slice(
        staged_choices,
        cand.choices_offset,
        cand.choices_len,
    );
    let nid = push_worker_candidate_node(arena, cand, state_handle, choices_offset, choices_len);

    if cand.turn_completed {
        next_turn_candidates.push(nid);
    } else {
        next_step_candidates.push(nid);
    }
}

#[allow(clippy::too_many_arguments)]
fn commit_wave_candidates(
    scratches: &mut [WorkerScratch],
    tt: &mut ShardedParetoTT,
    arena: &mut NodeArena,
    turn_arena: &mut MainStateArena,
    root_baseline: &RootEvalBaseline,
    policy: &SearchPolicy,
    incumbent_table: &PrimaryIncumbentTable,
    incumbent: &mut FrozenIncumbentBound,
    check_theft: bool,
    options: &PassSearchOptions,
    dop: usize,
    active_pool: Option<&rayon::ThreadPool>,
    active_parent_slices: &mut Vec<(u32, u16, u32, u32)>,
    tt_group_accepted: &mut [Vec<u32>; 16],
    tt_accepted_epoch: &mut Vec<u8>,
    tt_cur_epoch: &mut u8,
    tt_shard_buckets: &mut [Vec<(usize, StateHash, TranspositionLabel)>],
    tt_accepted_flags: &mut Vec<AtomicU8>,
    tt_flat_refs: &mut Vec<(usize, usize)>,
    completed: &mut Vec<NodeId>,
    terminal_tracker: &mut crate::eval::FinalPlanTracker,
    next_step_candidates: &mut Vec<NodeId>,
    next_turn_candidates: &mut Vec<NodeId>,
    mut defer_mid_turn: Option<&mut Vec<AcceptedMidTurn>>,
    transition_count: &mut u64,
    choice_replay_attempts: &mut u64,
    incumbent_pruned: &mut u64,
    por_pruned: &mut u64,
    branch_truncations: &mut u64,
    packed_state_bytes_total: &mut u64,
    has_victory: &mut bool,
    best_victory: &mut Option<BestVictoryState>,
    hp_target_reached: &mut bool,
    parent_count: usize,
    parent_slices_valid: bool,
    parent_slices: &[AtomicU64],
    sort_list: &mut Vec<(usize, usize, (u32, u32, u32))>,
) {
    let mut total_cands = 0usize;
    for wo in scratches.iter() {
        debug_assert_eq!(wo.candidates.len(), wo.cand_shards.len());
        debug_assert_eq!(wo.candidates.len(), wo.cand_fast_meta.len());
        total_cands += wo.candidates.len();
        *transition_count += wo.transitions;
        *choice_replay_attempts += wo.choice_replay_attempts;
        *incumbent_pruned += wo.incumbent_pruned;
        tt.branches_pruned += wo.tt_probe_pruned;
        *por_pruned += wo.por_pruned;
        *branch_truncations += wo.branch_truncations;
    }
    if let Some(buf) = defer_mid_turn.as_deref_mut() {
        buf.reserve(total_cands);
    } else {
        next_step_candidates.reserve(total_cands);
    }

    if let Some(pool) = active_pool.filter(|_| dop > 1 && total_cands >= 1024) {
        if parent_slices_valid {
            active_parent_slices.clear();
            let mut base_sort = 0u32;
            for slice_atom in &parent_slices[..parent_count] {
                let raw = slice_atom.load(Ordering::Relaxed);
                if raw == 0 {
                    continue;
                }
                let (w_idx, start, len) = unpack_parent_slice(raw);
                active_parent_slices.push((
                    base_sort,
                    w_idx as u16,
                    start as u32,
                    len as u32,
                ));
                base_sort += len as u32;
            }
            let scratches_ref: &[WorkerScratch] = &*scratches;
            pool.install(|| {
                tt.insert_grouped_parallel_from_slices(
                    active_parent_slices,
                    tt_group_accepted,
                    |w| scratches_ref[w].cand_shards.as_slice(),
                    |w, c| {
                        let cand = &scratches_ref[w].candidates[c];
                        (cand.state_hash, cand.label)
                    },
                );
            });
            *tt_cur_epoch = tt_cur_epoch.wrapping_add(1);
            if *tt_cur_epoch == 0 {
                tt_accepted_epoch.fill(0);
                *tt_cur_epoch = 1;
            }
            let epoch = *tt_cur_epoch;
            if tt_accepted_epoch.len() < total_cands {
                tt_accepted_epoch.resize(total_cands, 0);
            }
            for acc in tt_group_accepted.iter() {
                for &idx in acc {
                    tt_accepted_epoch[idx as usize] = epoch;
                }
            }
            #[cfg(debug_assertions)]
            let mut prev_sort_key: Option<(u32, u32, u32)> = None;
            for &(base_sort, w_idx_u16, start_u32, len_u32) in active_parent_slices.iter() {
                let w_idx = w_idx_u16 as usize;
                let wo = &scratches[w_idx];
                let start = start_u32 as usize;
                let len = len_u32 as usize;
                for i in 0..len {
                    let c_idx = start + i;
                    let sort_idx = base_sort as usize + i;
                    let fmeta = wo.cand_fast_meta[c_idx];
                    let is_terminal = (fmeta.flags & 1) != 0;
                    #[cfg(debug_assertions)]
                    {
                        let cand = &wo.candidates[c_idx];
                        debug_assert_eq!(is_terminal, cand.is_terminal);
                        debug_assert_eq!((fmeta.flags & 2) != 0, cand.turn_completed);
                        debug_assert!(
                            prev_sort_key.is_none_or(|p| p <= cand.sort_key),
                            "parent_slices commit out of sort_key order: {:?} > {:?}",
                            prev_sort_key,
                            cand.sort_key
                        );
                        prev_sort_key = Some(cand.sort_key);
                    }
                    if is_terminal || tt_accepted_epoch[sort_idx] == epoch {
                        if fmeta.flags == 0 {
                            if let Some(buf) = defer_mid_turn.as_deref_mut() {
                                buf.push(AcceptedMidTurn {
                                    w_idx: w_idx as u16,
                                    potion_count: fmeta.potion_count,
                                    explicit_potion_count: fmeta.explicit_potion_count,
                                    c_idx: c_idx as u32,
                                    beam_score: fmeta.beam_score,
                                });
                                continue;
                            }
                        }
                        let cand = &wo.candidates[c_idx];
                        commit_one_accepted(
                            w_idx,
                            c_idx,
                            cand,
                            &wo.slab,
                            &wo.staged_choices,
                            root_baseline,
                            policy,
                            incumbent_table,
                            incumbent,
                            check_theft,
                            options,
                            arena,
                            turn_arena,
                            completed,
                            terminal_tracker,
                            next_step_candidates,
                            next_turn_candidates,
                            defer_mid_turn.as_deref_mut(),
                            packed_state_bytes_total,
                            has_victory,
                            best_victory,
                            hp_target_reached,
                        );
                    }
                }
            }
        } else {
            tt_flat_refs.clear();
            tt_flat_refs.reserve(total_cands);
            for b in tt_shard_buckets.iter_mut() {
                b.clear();
            }
            sort_list.clear();
            for (w_idx, wo) in scratches.iter().enumerate() {
                for (c_idx, cand) in wo.candidates.iter().enumerate() {
                    sort_list.push((w_idx, c_idx, cand.sort_key));
                }
            }
            sort_list.sort_unstable_by_key(|&(_, _, sk)| sk);
            for &(w_idx, c_idx, _) in sort_list.iter() {
                let cand = &scratches[w_idx].candidates[c_idx];
                let sort_idx = tt_flat_refs.len();
                tt_flat_refs.push((w_idx, c_idx));
                if !cand.is_terminal {
                    tt_shard_buckets[cand.state_hash.shard_index()].push((
                        sort_idx,
                        cand.state_hash,
                        cand.label,
                    ));
                }
            }
            if tt_accepted_flags.len() < tt_flat_refs.len() {
                tt_accepted_flags.resize_with(tt_flat_refs.len(), || AtomicU8::new(0));
            }
            let flags_slice = &tt_accepted_flags[..tt_flat_refs.len()];
            pool.install(|| {
                tt.insert_sharded_batch_atomic_flags(tt_shard_buckets, true, flags_slice);
            });
            for (sort_idx, &(w_idx, c_idx)) in tt_flat_refs.iter().enumerate() {
                let wo = &scratches[w_idx];
                let cand = &wo.candidates[c_idx];
                if cand.is_terminal || flags_slice[sort_idx].load(Ordering::Relaxed) != 0 {
                    commit_one_accepted(
                        w_idx,
                        c_idx,
                        cand,
                        &wo.slab,
                        &wo.staged_choices,
                        root_baseline,
                        policy,
                        incumbent_table,
                        incumbent,
                        check_theft,
                        options,
                        arena,
                        turn_arena,
                        completed,
                        terminal_tracker,
                        next_step_candidates,
                        next_turn_candidates,
                        defer_mid_turn.as_deref_mut(),
                        packed_state_bytes_total,
                        has_victory,
                        best_victory,
                        hp_target_reached,
                    );
                }
            }
        }
    } else if parent_slices_valid {
        #[cfg(debug_assertions)]
        let mut prev_sort_key: Option<(u32, u32, u32)> = None;
        for slice_atom in &parent_slices[..parent_count] {
            let raw = slice_atom.load(Ordering::Relaxed);
            if raw == 0 {
                continue;
            }
            let (w_idx, start, len) = unpack_parent_slice(raw);
            let wo = &scratches[w_idx];
            let end = start + len;
            for c_idx in start..end {
                if c_idx + 4 < end && wo.cand_shards[c_idx + 4] != u16::MAX {
                    tt.prefetch(wo.candidates[c_idx + 4].state_hash);
                }
                let fmeta = wo.cand_fast_meta[c_idx];
                let is_terminal = (fmeta.flags & 1) != 0;
                let cand = &wo.candidates[c_idx];
                #[cfg(debug_assertions)]
                {
                    debug_assert_eq!(is_terminal, cand.is_terminal);
                    debug_assert_eq!((fmeta.flags & 2) != 0, cand.turn_completed);
                    debug_assert!(
                        prev_sort_key.is_none_or(|p| p <= cand.sort_key),
                        "parent_slices commit out of sort_key order: {:?} > {:?}",
                        prev_sort_key,
                        cand.sort_key
                    );
                    prev_sort_key = Some(cand.sort_key);
                }
                if is_terminal || tt.try_insert_single(cand.state_hash, cand.label) {
                    if fmeta.flags == 0 {
                        if let Some(buf) = defer_mid_turn.as_deref_mut() {
                            buf.push(AcceptedMidTurn {
                                w_idx: w_idx as u16,
                                potion_count: fmeta.potion_count,
                                explicit_potion_count: fmeta.explicit_potion_count,
                                c_idx: c_idx as u32,
                                beam_score: fmeta.beam_score,
                            });
                            continue;
                        }
                    }
                    commit_one_accepted(
                        w_idx,
                        c_idx,
                        cand,
                        &wo.slab,
                        &wo.staged_choices,
                        root_baseline,
                        policy,
                        incumbent_table,
                        incumbent,
                        check_theft,
                        options,
                        arena,
                        turn_arena,
                        completed,
                        terminal_tracker,
                        next_step_candidates,
                        next_turn_candidates,
                        defer_mid_turn.as_deref_mut(),
                        packed_state_bytes_total,
                        has_victory,
                        best_victory,
                        hp_target_reached,
                    );
                }
            }
        }
    } else {
        sort_list.clear();
        for (w_idx, wo) in scratches.iter().enumerate() {
            for (c_idx, cand) in wo.candidates.iter().enumerate() {
                sort_list.push((w_idx, c_idx, cand.sort_key));
            }
        }
        sort_list.sort_unstable_by_key(|&(_, _, sk)| sk);
        for &(w_idx, c_idx, _) in sort_list.iter() {
            let wo = &scratches[w_idx];
            let cand = &wo.candidates[c_idx];
            if cand.is_terminal || tt.try_insert_single(cand.state_hash, cand.label) {
                commit_one_accepted(
                    w_idx,
                    c_idx,
                    cand,
                    &wo.slab,
                    &wo.staged_choices,
                    root_baseline,
                    policy,
                    incumbent_table,
                    incumbent,
                    check_theft,
                    options,
                    arena,
                    turn_arena,
                    completed,
                    terminal_tracker,
                    next_step_candidates,
                    next_turn_candidates,
                    defer_mid_turn.as_deref_mut(),
                    packed_state_bytes_total,
                    has_victory,
                    best_victory,
                    hp_target_reached,
                );
            }
        }
    }
}

#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn materialize_promoted_victory(
    arena: &mut NodeArena,
    pid: NodeId,
    pv: crate::rollout::PromotedVictory,
    root_baseline: &RootEvalBaseline,
    incumbent_table: &PrimaryIncumbentTable,
    incumbent: &mut FrozenIncumbentBound,
    policy: &SearchPolicy,
    allow_paid_optional_potions: bool,
    check_theft: bool,
    check_hp_target: bool,
    has_victory: &mut bool,
    best_victory: &mut Option<BestVictoryState>,
    hp_target_reached: &mut bool,
    terminal_tracker: &mut crate::eval::FinalPlanTracker,
    completed: &mut Vec<NodeId>,
) {
    let pnode = arena.get(pid).clone();
    let mut cur_parent = pid;
    let total_acts = pv.steps.len();
    let mut cur_turn = pnode.turn as i32;
    let mut final_nid = pid;
    let final_rare = RareNodeEval {
        growth_rewards: pv.final_fast.growth_rewards,
        relic_eval: pv.final_fast.relic_eval,
    };
    for (step_idx, step) in pv.steps.iter().enumerate() {
        let is_last = step_idx + 1 == total_acts;
        let (kind, step_turn) = if step.is_end_turn != 0 {
            let t = cur_turn;
            cur_turn += 1;
            (CompactActionKind::EndTurn, t)
        } else {
            (
                CompactActionKind::PlayCard {
                    card_handle: step.card_handle,
                    target_handle: if step.target_handle == u32::MAX {
                        None
                    } else {
                        Some(step.target_handle)
                    },
                },
                cur_turn,
            )
        };
        let (c_off, c_len) = arena.choices.push_raw_slab_slice(
            &pv.choices.steps_meta,
            &pv.choices.indices,
            step.choices_offset,
            step.choices_len,
        );
        let node_turn = if is_last { pv.final_turn } else { step_turn };
        let rare_eval_idx = if is_last {
            arena.push_rare_eval(final_rare)
        } else {
            pnode.rare_eval_idx
        };
        let (step_action, step_por_key) =
            CompactAction::new(kind, step_turn, PorActionClass::Barrier);
        let nid = arena.push_node(SearchNode {
            parent_raw: cur_parent.0,
            action: step_action,
            por_key: step_por_key,
            choices_offset: c_off,
            choices_len: c_len,
            state_handle: PackedHandle::default(),
            state_hash: if is_last {
                pv.final_state_hash
            } else {
                pnode.state_hash
            },
            beam_rank_score: if is_last {
                pv.final_fast.base_score
            } else {
                pnode.beam_rank_score
            },
            turn: node_turn.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            action_count: pnode.action_count + step_idx as u16 + 1,
            potion_count: if is_last {
                (pnode.potion_count as u16 + pv.auto_potions_used).min(u8::MAX as u16) as u8
            } else {
                pnode.potion_count
            },
            explicit_potion_count: pnode.explicit_potion_count,
            optional_paid_potion_count: pnode.optional_paid_potion_count,
            automatic_potion_count: if is_last {
                (pnode.automatic_potion_count as u16 + pv.auto_potions_used)
                    .min(u8::MAX as u16) as u8
            } else {
                pnode.automatic_potion_count
            },
            used_potion_slot_mask: pnode.used_potion_slot_mask,
            potion_strategic_cost: pnode.potion_strategic_cost,
            optional_paid_potion_cost: pnode.optional_paid_potion_cost,
            ambergris_count: pnode.ambergris_count,
            forced_used_mask: pnode.forced_used_mask,
            forced_ambergris_count: pnode.forced_ambergris_count,
            node_flags: if is_last { NODE_FLAG_IS_TERMINAL } else { 0 },
            fast_eval: if is_last {
                StoredFastEval::from(&pv.final_fast)
            } else {
                pnode.fast_eval
            },
            rare_eval_idx,
            ..Default::default()
        });
        cur_parent = nid;
        final_nid = nid;
    }
    let promoted_fast = StoredFastEval::from(&pv.final_fast);
    record_victory_and_tighten(
        root_baseline,
        incumbent_table,
        incumbent,
        policy,
        &promoted_fast,
        &final_rare,
        pv.final_turn,
        pnode.potion_count as u16 + pv.auto_potions_used,
        pnode.explicit_potion_count as u16,
        pnode.optional_paid_potion_count as u16,
        pnode.potion_strategic_cost as i32,
        pnode.optional_paid_potion_cost as i32,
        pnode.forced_used_mask,
        pv.final_fast.base_score.round() as i64,
        has_victory,
        best_victory,
    );
    if check_hp_target
        && has_reached_acceptable_battle_hp_loss_node(root_baseline, policy, arena, final_nid)
        && (!allow_paid_optional_potions || {
            let promoted_rare = arena.rare_eval(final_nid);
            let promoted_net_cost = PotionUsePolicy::compute_net_potion_opportunity_cost_centi(
                pnode.optional_paid_potion_cost as i32,
                pnode.optional_paid_potion_count as i32,
                pv.final_fast.alchemize_count as i32,
                pv.final_fast.empty_potion_slots as i32,
                root_baseline,
            );
            !PotionUsePolicy::can_any_paid_potion_improve_on(
                PaidPotionImproveInput {
                    won: true,
                    survives: true,
                    death_save_use_count: pv.final_fast.death_save_use_count,
                    end_player_hp: pv.final_fast.player_hp,
                    end_player_max_hp: pv.final_fast.player_max_hp,
                    used_potion_slot_mask: pnode.used_potion_slot_mask,
                    optional_paid_potion_count: pnode.optional_paid_potion_count,
                    raw_potion_strategic_cost: pnode.optional_paid_potion_cost as i32,
                    net_potion_opportunity_cost_centi: promoted_net_cost,
                    alchemize_count: pv.final_fast.alchemize_count,
                    empty_potion_slots: pv.final_fast.empty_potion_slots,
                    all_strategy_targets_satisfied: are_strategy_targets_satisfied_raw(
                        root_baseline,
                        &promoted_fast,
                        &promoted_rare,
                    ),
                    all_growth_targets_satisfied: !root_baseline.effective_has_growth_targets
                        || root_baseline
                            .growth_opportunity_targets
                            .is_satisfied_by(&promoted_rare.growth_rewards),
                    growth_hp_credit: pv.final_fast.strategy_goal_hp_credit,
                    relic_satisfied_mask: promoted_rare.relic_eval.satisfied_mask as u32,
                    outstanding_stolen: pv.final_fast.outstanding_stolen,
                    forfeits_gold: pv.final_fast.forfeits_gold,
                    theft_preserve_resources: check_theft,
                },
                root_baseline,
            )
        })
    {
        *hp_target_reached = true;
    }
    terminal_tracker.observe_node(arena, final_nid, root_baseline);
    completed.push(final_nid);
}

#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn init_beam_pass_root(
    root: &BattleState,
    content: &dyn ContentDispatcher,
    policy: &SearchPolicy,
    profile: &SolverSearchProfile,
    eval_mode: EvalMode,
    fixed_prefix: Option<&[PlannedAction]>,
    root_baseline: &RootEvalBaseline,
    pack_buf: &mut Vec<u8>,
    arena: &mut NodeArena,
    step_arena: &mut MainStateArena,
    completed: &mut Vec<NodeId>,
    current_turn_frontier: &mut Vec<NodeId>,
    terminal_tracker: &mut crate::eval::FinalPlanTracker,
) -> Result<(BattleState, NodeId, i32, i32, i32, u64), KernelError> {
    let (
        start_state,
        prefix_potions,
        prefix_optional_paid_potions,
        prefix_potion_cost,
        prefix_optional_paid_cost,
        prefix_ambergris,
        prefix_used_slot_mask,
        prefix_forced_mask,
        prefix_forced_amb,
    ) = if let Some(prefix) = fixed_prefix {
        replay_fixed_prefix(root, prefix, policy, root_baseline, content)?
    } else {
        (
            root.clone(),
            0u16,
            0u16,
            0i32,
            0i32,
            0u16,
            0u16,
            0u64,
            0u16,
        )
    };

    arena.clear();
    let prefix_len = fixed_prefix.map_or(0, |p| p.len() as u16);
    pack_state(&start_state, pack_buf);
    let packed_state_bytes_total = pack_buf.len() as u64;
    let root_hash = hash_canonical_key(&start_state);
    let root_state_handle = step_arena.commit_slice(pack_buf);
    let (root_fast, mut root_full, _) =
        evaluate_fast_and_full_fused(&start_state, content, root_baseline, 0, 0, None, None);
    let prefix_net_potion_cost_centi = PotionUsePolicy::compute_net_potion_opportunity_cost_centi(
        prefix_optional_paid_cost,
        prefix_optional_paid_potions as i32,
        root_fast.alchemize_count as i32,
        root_fast.empty_potion_slots as i32,
        root_baseline,
    );
    root_full.beam_rank_score = compute_beam_rank_score_with_centi(
        &root_fast,
        &root_full,
        root_baseline,
        profile.base_score_only,
        profile.boss_tempo_hp_pricing,
        profile.beam_weight_perturbation,
        eval_mode,
        prefix_net_potion_cost_centi,
        prefix_len,
    );
    let best_seen_max_dark_evoke = root_full.max_dark_evoke;
    let best_seen_poison_and_doom = root_full.total_poison_and_doom;
    let best_seen_board_investment = root_full.board_investment;
    let root_rare_idx = arena.push_rare_eval(RareNodeEval {
        growth_rewards: root_fast.growth_rewards,
        relic_eval: root_fast.relic_eval,
    });
    let root_terminal = !start_state.is_in_progress
        || root_fast.won
        || root_fast.player_dead
        || root_fast.is_event_defeat;
    let mut root_node = SearchNode {
        state_handle: root_state_handle,
        state_hash: root_hash,
        beam_rank_score: root_fast.base_score,
        turn: start_state
            .player
            .turn_number
            .clamp(i16::MIN as i32, i16::MAX as i32) as i16,
        action_count: prefix_len,
        potion_count: prefix_potions.min(u8::MAX as u16) as u8,
        explicit_potion_count: prefix_potions.min(u8::MAX as u16) as u8,
        optional_paid_potion_count: prefix_optional_paid_potions.min(u8::MAX as u16) as u8,
        used_potion_slot_mask: prefix_used_slot_mask,
        potion_strategic_cost: prefix_potion_cost.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
        optional_paid_potion_cost: prefix_optional_paid_cost
            .clamp(i16::MIN as i32, i16::MAX as i32) as i16,
        ambergris_count: prefix_ambergris.min(u8::MAX as u16) as u8,
        forced_used_mask: prefix_forced_mask,
        forced_ambergris_count: prefix_forced_amb.min(u8::MAX as u16) as u8,
        node_flags: if root_terminal {
            NODE_FLAG_IS_TERMINAL
        } else {
            0
        },
        fast_eval: StoredFastEval::from(&root_fast),
        rare_eval_idx: root_rare_idx,
        ..Default::default()
    };
    root_node.set_full_eval(&root_full.to_stored());
    let root_id = arena.push_node(root_node);
    if arena.get(root_id).is_terminal() {
        terminal_tracker.observe_node(arena, root_id, root_baseline);
        completed.push(root_id);
    } else {
        current_turn_frontier.push(root_id);
    }
    Ok((
        start_state,
        root_id,
        best_seen_max_dark_evoke,
        best_seen_poison_and_doom,
        best_seen_board_investment,
        packed_state_bytes_total,
    ))
}

#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn expand_depth0_turn_setup(
    start_state: &BattleState,
    root_id: NodeId,
    prefix_len: u16,
    prefix_potions: u16,
    prefix_optional_paid_potions: u16,
    prefix_used_slot_mask: u16,
    prefix_potion_cost: i32,
    prefix_optional_paid_cost: i32,
    prefix_ambergris: u16,
    prefix_forced_mask: u64,
    prefix_forced_amb: u16,
    started: Instant,
    beam_width: usize,
    check_theft: bool,
    dop: usize,
    active_pool: Option<&rayon::ThreadPool>,
    cancel: &AtomicBool,
    abort: &SearchAbortSlot,
    content: &(dyn ContentDispatcher + Sync),
    root_baseline: &RootEvalBaseline,
    policy: &SearchPolicy,
    profile: &SolverSearchProfile,
    cert: &ClosureCertificate,
    incumbent_table: &PrimaryIncumbentTable,
    incumbent: &mut FrozenIncumbentBound,
    sensitivity: &CombatOrderSensitivity,
    options: &PassSearchOptions,
    scratches: &mut [WorkerScratch],
    deferred_batches: &mut Vec<DeferredChoiceBatch>,
    probe_jobs: &mut Vec<ProbeJob>,
    rollout_workspaces: &mut [crate::rollout::RolloutWorkspace],
    rollout_results: &mut Vec<Option<RolloutResult>>,
    turn_rollout_cache: &TurnRolloutCache,
    choice_probe_cache: &mut ChoiceProbeCache,
    tt: &mut ShardedParetoTT,
    arena: &mut NodeArena,
    step_arena: &mut MainStateArena,
    turn_staging_arena: &mut MainStateArena,
    prune_scratch: &mut PruneScratch,
    active_parent_slices: &mut Vec<(u32, u16, u32, u32)>,
    tt_group_accepted: &mut [Vec<u32>; 16],
    tt_accepted_epoch: &mut Vec<u8>,
    tt_cur_epoch: &mut u8,
    tt_shard_buckets: &mut [Vec<(usize, StateHash, TranspositionLabel)>],
    tt_accepted_flags: &mut Vec<AtomicU8>,
    tt_flat_refs: &mut Vec<(usize, usize)>,
    parent_slices: &mut Vec<AtomicU64>,
    sort_list: &mut Vec<(usize, usize, (u32, u32, u32))>,
    completed: &mut Vec<NodeId>,
    terminal_tracker: &mut crate::eval::FinalPlanTracker,
    next_step_candidates: &mut Vec<NodeId>,
    turn_setup_ended_turn: &mut Vec<NodeId>,
    shortlist_buf: &mut Vec<NodeId>,
    current_turn_frontier: &mut Vec<NodeId>,
    transition_count: &mut u64,
    choice_replay_attempts: &mut u64,
    incumbent_pruned: &mut u64,
    por_pruned: &mut u64,
    branch_truncations: &mut u64,
    any_lossy_truncation: &mut bool,
    packed_state_bytes_total: &mut u64,
    has_victory: &mut bool,
    best_victory: &mut Option<BestVictoryState>,
    termination: &mut &'static str,
) -> Result<(), KernelError> {
    for s in scratches.iter_mut() {
        if s.wave_dirty {
            s.reset_for_wave();
        }
    }
    scratches[0].wave_dirty = true;
    let filled_at_start = start_state
        .player
        .potion_slots
        .iter()
        .filter(|s| s.is_some())
        .count()
        .min(u8::MAX as usize) as u8;
    let setup_task = ActionTask {
        parent_rank: 0,
        parent_id: root_id,
        parent_turn: start_state.player.turn_number,
        parent_action_count: prefix_len,
        parent_potion_count: prefix_potions,
        parent_explicit_potion_count: prefix_potions,
        parent_optional_paid_potion_count: prefix_optional_paid_potions,
        parent_automatic_potion_count: 0,
        parent_filled_potions: filled_at_start,
        parent_used_potion_slot_mask: prefix_used_slot_mask,
        parent_potion_cost: prefix_potion_cost,
        parent_optional_paid_potion_cost: prefix_optional_paid_cost,
        parent_ambergris_count: prefix_ambergris,
        parent_forced_used_mask: prefix_forced_mask,
        parent_forced_ambergris_count: prefix_forced_amb,
        parent_has_choice_lookahead: false,
        parent_choice_lookahead_score: None,
        action_ord: 0,
        spec: ActionSpec::TurnSetup,
    };
    let frozen_setup_inc = *incumbent;
    let mut setup_hp_target = false;
    if parent_slices.is_empty() {
        parent_slices.push(AtomicU64::new(0));
    }
    parent_slices[0].store(0, Ordering::Relaxed);
    let defer_probes = dop > 1 && active_pool.is_some();
    let turn_boundary_rollouts_enabled = options.rollout_horizon_turns > 0;
    let deferred = if defer_probes {
        let mut probe_access = ProbeCacheAccess::Shared(choice_probe_cache);
        execute_action_from_parent(
            &mut scratches[0],
            start_state,
            &setup_task,
            0,
            true,
            turn_boundary_rollouts_enabled,
            &start_state.static_ctx,
            content,
            root_baseline,
            policy,
            profile,
            cert,
            &frozen_setup_inc,
            tt,
            &mut probe_access,
            sensitivity,
            options.allow_paid_optional_potions,
            options.eval_mode,
        )
    } else {
        let mut probe_access = ProbeCacheAccess::SingleThread(choice_probe_cache);
        execute_action_from_parent(
            &mut scratches[0],
            start_state,
            &setup_task,
            0,
            false,
            turn_boundary_rollouts_enabled,
            &start_state.static_ctx,
            content,
            root_baseline,
            policy,
            profile,
            cert,
            &frozen_setup_inc,
            tt,
            &mut probe_access,
            sensitivity,
            options.allow_paid_optional_potions,
            options.eval_mode,
        )
    };
    if let Some(err) = scratches[0].fatal_error.take() {
        abort.record_error(err);
        return Err(abort.clone_error().expect("abort error recorded"));
    }
    if deferred {
        resolve_deferred_choice_batches_on_pool(
            dop,
            active_pool,
            scratches,
            deferred_batches,
            probe_jobs,
            choice_probe_cache,
            &start_state.static_ctx,
            content,
            root_baseline,
            profile,
            options.eval_mode,
            cancel,
            &parent_slices[..1],
            true,
        );
    } else {
        let setup_len = scratches[0].candidates.len();
        if setup_len > 0 {
            parent_slices[0].store(pack_parent_slice(0, 0, setup_len), Ordering::Relaxed);
        }
    }
    next_step_candidates.clear();
    turn_setup_ended_turn.clear();
    commit_wave_candidates(
        scratches,
        tt,
        arena,
        turn_staging_arena,
        root_baseline,
        policy,
        incumbent_table,
        incumbent,
        check_theft,
        options,
        dop,
        active_pool,
        active_parent_slices,
        tt_group_accepted,
        tt_accepted_epoch,
        tt_cur_epoch,
        tt_shard_buckets,
        tt_accepted_flags,
        tt_flat_refs,
        completed,
        terminal_tracker,
        next_step_candidates,
        turn_setup_ended_turn,
        None,
        transition_count,
        choice_replay_attempts,
        incumbent_pruned,
        por_pruned,
        branch_truncations,
        packed_state_bytes_total,
        has_victory,
        best_victory,
        &mut setup_hp_target,
        1,
        true,
        parent_slices,
        sort_list,
    );

    current_turn_frontier.clear();
    if setup_hp_target {
        *termination = "HpTarget";
    } else if !next_step_candidates.is_empty() {
        let has_time_for_setup_rollout = profile.soft_time_budget_ms == 0
            || profile
                .soft_time_budget_ms
                .saturating_sub(started.elapsed().as_millis() as u64)
                >= 150;
        let will_run_setup_rollout = next_step_candidates.len() > beam_width
            && options.rollout_horizon_turns > 0
            && !cancel.load(Ordering::Relaxed)
            && has_time_for_setup_rollout;

        if will_run_setup_rollout {
            let shortlist_cap = next_step_candidates.len().min(beam_width * 2);
            if next_step_candidates.len() > shortlist_cap {
                BeamRetentionPolicy::prune_wave_with_rollout_into(
                    arena,
                    next_step_candidates,
                    shortlist_cap,
                    profile.potion_free_beam_width,
                    profile.potion_beam_width,
                    root_baseline,
                    profile.second_rank_band,
                    profile.aggressive_power_commitment,
                    false,
                    false,
                    active_pool,
                    shortlist_buf,
                    prune_scratch,
                );
            } else {
                shortlist_buf.clear();
                shortlist_buf.extend_from_slice(next_step_candidates);
            }
            step_arena.clear();
            for &nid in shortlist_buf.iter() {
                let tmp_h = arena.get(nid).state_handle;
                let wo = &scratches[tmp_h.slab as usize];
                let cand = &wo.candidates[tmp_h.offset as usize];
                let bytes = wo.slab.get(cand.state_slab_idx, cand.state_off, cand.state_len);
                *packed_state_bytes_total += bytes.len() as u64;
                arena.get_mut(nid).state_handle = step_arena.commit_slice(bytes);
            }
            let rollout_deadline = (profile.soft_time_budget_ms > 0)
                .then(|| started + Duration::from_millis(profile.soft_time_budget_ms));
            evaluate_turn_boundary_rollouts_into(
                step_arena,
                arena,
                shortlist_buf,
                &start_state.static_ctx,
                content,
                root_baseline,
                policy,
                profile,
                options.eval_mode,
                2,
                rollout_deadline,
                active_pool,
                cancel,
                rollout_workspaces,
                Some(turn_rollout_cache),
                rollout_results,
            );
            for (idx, maybe_rr) in rollout_results.drain(..).enumerate() {
                let Some(rr) = maybe_rr else { continue };
                let pid = shortlist_buf[idx];
                arena.get_mut(pid).set_rollout_score(rr.rollout_score);
                if let Some(pv) = rr.promoted_victory {
                    materialize_promoted_victory(
                        arena,
                        pid,
                        pv,
                        root_baseline,
                        incumbent_table,
                        incumbent,
                        policy,
                        options.allow_paid_optional_potions,
                        check_theft,
                        false,
                        has_victory,
                        best_victory,
                        &mut setup_hp_target,
                        terminal_tracker,
                        completed,
                    );
                }
            }
            BeamRetentionPolicy::prune_wave_with_rollout_into(
                arena,
                shortlist_buf,
                beam_width,
                profile.potion_free_beam_width,
                profile.potion_beam_width,
                root_baseline,
                profile.second_rank_band,
                profile.aggressive_power_commitment,
                false,
                true,
                active_pool,
                current_turn_frontier,
                prune_scratch,
            );
            shortlist_buf.clear();
            if current_turn_frontier.len() < next_step_candidates.len()
                || next_step_candidates.len() > beam_width
            {
                *any_lossy_truncation = true;
            }
        } else {
            BeamRetentionPolicy::prune_wave_with_rollout_into(
                arena,
                next_step_candidates,
                beam_width,
                profile.potion_free_beam_width,
                profile.potion_beam_width,
                root_baseline,
                profile.second_rank_band,
                profile.aggressive_power_commitment,
                false,
                false,
                active_pool,
                current_turn_frontier,
                prune_scratch,
            );
            if current_turn_frontier.len() < next_step_candidates.len()
                || next_step_candidates.len() > beam_width
            {
                *any_lossy_truncation = true;
            }
            step_arena.clear();
            for &nid in current_turn_frontier.iter() {
                let tmp_h = arena.get(nid).state_handle;
                let wo = &scratches[tmp_h.slab as usize];
                let cand = &wo.candidates[tmp_h.offset as usize];
                let bytes = wo.slab.get(cand.state_slab_idx, cand.state_off, cand.state_len);
                *packed_state_bytes_total += bytes.len() as u64;
                arena.get_mut(nid).state_handle = step_arena.commit_slice(bytes);
            }
        }
    }
    Ok(())
}

#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn flush_stand_pat_end_turn(
    active_frontier: &[NodeId],
    stand_pat_limit: usize,
    frontier_in_worker_slabs: bool,
    check_theft: bool,
    dop: usize,
    active_pool: Option<&rayon::ThreadPool>,
    cancel: &AtomicBool,
    abort: &SearchAbortSlot,
    static_ctx: &Arc<RootStaticContext>,
    content: &(dyn ContentDispatcher + Sync),
    root_baseline: &RootEvalBaseline,
    policy: &SearchPolicy,
    profile: &SolverSearchProfile,
    cert: &ClosureCertificate,
    incumbent_table: &PrimaryIncumbentTable,
    incumbent: &mut FrozenIncumbentBound,
    sensitivity: &CombatOrderSensitivity,
    options: &PassSearchOptions,
    scratches: &mut [WorkerScratch],
    parent_states: &mut [BattleState],
    deferred_batches: &mut Vec<DeferredChoiceBatch>,
    probe_jobs: &mut Vec<ProbeJob>,
    stand_pat_seen: &mut hashbrown::HashSet<(StateHash, StandPatParentKey), ahash::RandomState>,
    choice_probe_cache: &mut ChoiceProbeCache,
    tt: &mut ShardedParetoTT,
    arena: &mut NodeArena,
    step_arena: &MainStateArena,
    prev_worker_slabs: &[LocalStateSlab],
    turn_staging_arena: &mut MainStateArena,
    active_parent_slices: &mut Vec<(u32, u16, u32, u32)>,
    tt_group_accepted: &mut [Vec<u32>; 16],
    tt_accepted_epoch: &mut Vec<u8>,
    tt_cur_epoch: &mut u8,
    tt_shard_buckets: &mut [Vec<(usize, StateHash, TranspositionLabel)>],
    tt_accepted_flags: &mut Vec<AtomicU8>,
    tt_flat_refs: &mut Vec<(usize, usize)>,
    parent_slices: &mut Vec<AtomicU64>,
    sort_list: &mut Vec<(usize, usize, (u32, u32, u32))>,
    completed: &mut Vec<NodeId>,
    terminal_tracker: &mut crate::eval::FinalPlanTracker,
    sp_mid_unused: &mut Vec<NodeId>,
    next_turn_candidates: &mut Vec<NodeId>,
    transition_count: &mut u64,
    choice_replay_attempts: &mut u64,
    incumbent_pruned: &mut u64,
    por_pruned: &mut u64,
    branch_truncations: &mut u64,
    packed_state_bytes_total: &mut u64,
    has_victory: &mut bool,
    best_victory: &mut Option<BestVictoryState>,
    hp_target_reached: &mut bool,
    termination: &mut &'static str,
) -> Result<(), KernelError> {
    let sp_ids = select_stand_pat_candidates(
        active_frontier,
        arena,
        stand_pat_limit,
        options.prefer_potion_buckets,
    );
    let mut uncached_sp_ids: SmallVec<[NodeId; 16]> = SmallVec::new();
    for nid in sp_ids {
        let pnode = arena.get(nid);
        if stand_pat_seen.insert((pnode.state_hash, StandPatParentKey::from_node(pnode))) {
            uncached_sp_ids.push(nid);
        }
    }
    if uncached_sp_ids.is_empty() {
        return Ok(());
    }

    for s in scratches.iter_mut() {
        if s.wave_dirty {
            s.reset_for_wave();
        }
    }
    let frozen_incumbent = *incumbent;
    let turn_boundary_rollouts_enabled = options.rollout_horizon_turns > 0;
    let sp_slice = uncached_sp_ids.as_slice();
    if parent_slices.len() < sp_slice.len() {
        parent_slices.resize_with(sp_slice.len(), || AtomicU64::new(0));
    }
    for slot in &parent_slices[..sp_slice.len()] {
        slot.store(0, Ordering::Relaxed);
    }
    let sp_parent_source = if frontier_in_worker_slabs {
        ParentStateSource::WorkerSlabs(prev_worker_slabs)
    } else {
        ParentStateSource::Main(step_arena)
    };

    if let Some(pool) = active_pool.filter(|_| dop > 1 && sp_slice.len() > 2) {
        let workers = dop.min(sp_slice.len()).min(scratches.len()).min(parent_states.len());
        let next_idx = AtomicUsize::new(0);
        let arena_ref: &NodeArena = &*arena;
        let tt_ref: &ShardedParetoTT = &*tt;
        let probe_cache_ref: &ChoiceProbeCache = &*choice_probe_cache;
        let slices_ref = &parent_slices[..sp_slice.len()];
        pool.scope(|scope| {
            for (w_idx, (scratch, parent_state)) in scratches
                .iter_mut()
                .zip(parent_states.iter_mut())
                .take(workers)
                .enumerate()
            {
                let next_ref = &next_idx;
                scope.spawn(move |_| {
                    scratch.wave_dirty = true;
                    let mut probe_access = ProbeCacheAccess::Shared(probe_cache_ref);
                    loop {
                        if cancel.load(Ordering::Relaxed) || abort.is_tripped() {
                            break;
                        }
                        let rank = next_ref.fetch_add(1, Ordering::Relaxed);
                        if rank >= sp_slice.len() {
                            break;
                        }
                        let nid = sp_slice[rank];
                        let pnode = arena_ref.get(nid);
                        let pbytes = sp_parent_source.get(pnode.state_handle);
                        // SAFETY: `pbytes` was committed from `pack_state` output in this process.
                        if unsafe { unpack_state(pbytes, static_ctx, parent_state) }.is_err() {
                            continue;
                        }
                        let filled = parent_state
                            .player
                            .potion_slots
                            .iter()
                            .filter(|s| s.is_some())
                            .count()
                            .min(u8::MAX as usize) as u8;
                        let task = make_stand_pat_task(rank as u32, nid, pnode, filled, 0);
                        let c_start = scratch.candidates.len();
                        let deferred = execute_action_from_parent(
                            scratch,
                            parent_state,
                            &task,
                            rank,
                            true,
                            turn_boundary_rollouts_enabled,
                            static_ctx,
                            content,
                            root_baseline,
                            policy,
                            profile,
                            cert,
                            &frozen_incumbent,
                            tt_ref,
                            &mut probe_access,
                            sensitivity,
                            options.allow_paid_optional_potions,
                            options.eval_mode,
                        );
                        if let Some(err) = scratch.fatal_error.take() {
                            abort.record_error(err);
                            break;
                        }
                        if !deferred {
                            let c_len = scratch.candidates.len() - c_start;
                            if c_len > 0 {
                                slices_ref[rank]
                                    .store(pack_parent_slice(w_idx, c_start, c_len), Ordering::Relaxed);
                            }
                        }
                    }
                });
            }
        });
        if !abort.is_tripped() {
            resolve_deferred_choice_batches_on_pool(
                dop,
                Some(pool),
                scratches,
                deferred_batches,
                probe_jobs,
                choice_probe_cache,
                static_ctx,
                content,
                root_baseline,
                profile,
                options.eval_mode,
                cancel,
                &parent_slices[..sp_slice.len()],
                true,
            );
        }
    } else {
        scratches[0].wave_dirty = true;
        let mut probe_access = ProbeCacheAccess::SingleThread(choice_probe_cache);
        for (rank, &nid) in sp_slice.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) || abort.is_tripped() {
                break;
            }
            let pnode = arena.get(nid).clone();
            let pbytes = sp_parent_source.get(pnode.state_handle);
            // SAFETY: `pbytes` was committed from `pack_state` output in this process.
            if unsafe { unpack_state(pbytes, static_ctx, &mut parent_states[0]) }.is_err() {
                continue;
            }
            let filled = parent_states[0]
                .player
                .potion_slots
                .iter()
                .filter(|s| s.is_some())
                .count()
                .min(u8::MAX as usize) as u8;
            let task = make_stand_pat_task(rank as u32, nid, &pnode, filled, 0);
            let c_start = scratches[0].candidates.len();
            let _ = execute_action_from_parent(
                &mut scratches[0],
                &parent_states[0],
                &task,
                rank,
                false,
                turn_boundary_rollouts_enabled,
                static_ctx,
                content,
                root_baseline,
                policy,
                profile,
                cert,
                &frozen_incumbent,
                tt,
                &mut probe_access,
                sensitivity,
                options.allow_paid_optional_potions,
                options.eval_mode,
            );
            if let Some(err) = scratches[0].fatal_error.take() {
                abort.record_error(err);
                break;
            }
            let c_len = scratches[0].candidates.len() - c_start;
            if c_len > 0 {
                parent_slices[rank].store(pack_parent_slice(0, c_start, c_len), Ordering::Relaxed);
            }
        }
    }
    if let Some(err) = abort.clone_error() {
        return Err(err);
    }
    sp_mid_unused.clear();
    commit_wave_candidates(
        scratches,
        tt,
        arena,
        turn_staging_arena,
        root_baseline,
        policy,
        incumbent_table,
        incumbent,
        check_theft,
        options,
        dop,
        active_pool,
        active_parent_slices,
        tt_group_accepted,
        tt_accepted_epoch,
        tt_cur_epoch,
        tt_shard_buckets,
        tt_accepted_flags,
        tt_flat_refs,
        completed,
        terminal_tracker,
        sp_mid_unused,
        next_turn_candidates,
        None,
        transition_count,
        choice_replay_attempts,
        incumbent_pruned,
        por_pruned,
        branch_truncations,
        packed_state_bytes_total,
        has_victory,
        best_victory,
        hp_target_reached,
        sp_slice.len(),
        true,
        parent_slices,
        sort_list,
    );
    if *hp_target_reached {
        *termination = "HpTarget";
    }
    Ok(())
}

#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn emit_search_progress(
    cb: &(dyn Fn(&SearchProgress) + Sync),
    started: Instant,
    last_progress_ms: &mut Option<u64>,
    expanded_nodes: usize,
    transition_count: u64,
    choice_replay_attempts: u64,
    current_turn: i32,
    has_victory: bool,
    best_won_hp_lost: Option<i32>,
) {
    let elapsed_ms = started.elapsed().as_millis() as u64;
    if last_progress_ms.is_none_or(|prev| elapsed_ms.saturating_sub(prev) >= 100) {
        *last_progress_ms = Some(elapsed_ms);
        cb(&SearchProgress {
            nodes_expanded: expanded_nodes,
            transition_count,
            choice_replay_attempts,
            current_turn,
            best_won: has_victory,
            best_hp_lost: best_won_hp_lost,
            elapsed_ms,
        });
    }
}

#[cold]
#[inline(never)]
fn collect_early_turn_scout_layer(
    arena: &NodeArena,
    next_turn_candidates: &[NodeId],
    start_turn_number: i32,
    completed_turns: i32,
    early_turn_scout_depth: i32,
    early_turn_frontiers: &mut Vec<crate::early_turn::EarlyTurnFrontierCandidate>,
    termination: &mut &'static str,
) -> bool {
    let f = select_early_turn_frontier(
        arena,
        next_turn_candidates,
        start_turn_number,
        completed_turns,
    );
    early_turn_frontiers.extend(f);
    if completed_turns >= early_turn_scout_depth {
        *termination = "ScoutDepth";
        true
    } else {
        false
    }
}

#[cold]
#[inline(never)]
fn collect_boss_tempo_scout_prefixes(
    arena: &NodeArena,
    next_turn_candidates: &[NodeId],
    root_baseline: &RootEvalBaseline,
    fixed_prefix: Option<&[PlannedAction]>,
    boss_tempo_prefixes: &mut Vec<BossTempoPrefix>,
    termination: &mut &'static str,
) {
    for &nid in next_turn_candidates {
        let n = arena.get(nid);
        if !n.fast_eval.player_dead() {
            let eliminated =
                (root_baseline.initial_enemy_count as i32) - (n.fast_eval.alive_enemy_count as i32);
            let rank = n.beam_rank_score_or_base();
            let mut actions = fixed_prefix.map_or_else(Vec::new, |p| p.to_vec());
            actions.extend(arena.reconstruct_actions(nid));
            let forced_uses_satisfied = (n.forced_used_mask & root_baseline.all_forced_potion_mask)
                == root_baseline.all_forced_potion_mask;
            retain_boss_tempo_prefixes(
                boss_tempo_prefixes,
                BossTempoPrefix {
                    actions,
                    state_hash: n.state_hash,
                    forced_uses_satisfied,
                    eliminated_enemies: eliminated,
                    potion_count: n.potion_count as usize,
                    rank,
                },
            );
        }
    }
    *termination = "BossTempoScout";
}

#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn run_turn_boundary_rollouts_and_promote(
    started: Instant,
    effective_turn_boundary_width: usize,
    next_turn_number: i32,
    is_elite_or_boss: bool,
    check_theft: bool,
    active_pool: Option<&rayon::ThreadPool>,
    cancel: &AtomicBool,
    static_ctx: &Arc<RootStaticContext>,
    content: &(dyn ContentDispatcher + Sync),
    root_baseline: &RootEvalBaseline,
    policy: &SearchPolicy,
    profile: &SolverSearchProfile,
    incumbent_table: &PrimaryIncumbentTable,
    incumbent: &mut FrozenIncumbentBound,
    options: &PassSearchOptions,
    arena: &mut NodeArena,
    turn_staging_arena: &MainStateArena,
    prune_scratch: &mut PruneScratch,
    next_turn_candidates: &mut Vec<NodeId>,
    shortlist_buf: &mut Vec<NodeId>,
    rollout_workspaces: &mut [crate::rollout::RolloutWorkspace],
    turn_rollout_cache: &TurnRolloutCache,
    rollout_results: &mut Vec<Option<RolloutResult>>,
    has_victory: &mut bool,
    best_victory: &mut Option<BestVictoryState>,
    hp_target_reached: &mut bool,
    hit_post_victory_turn_cap: &mut bool,
    completed: &mut Vec<NodeId>,
    terminal_tracker: &mut crate::eval::FinalPlanTracker,
) -> bool {
    let has_time_for_rollout = profile.soft_time_budget_ms == 0
        || profile
            .soft_time_budget_ms
            .saturating_sub(started.elapsed().as_millis() as u64)
            >= 150;
    let has_any_strategy_targets = root_baseline.effective_has_growth_targets
        || !root_baseline.active_relic_targets.is_empty();
    let needs_turn_boundary_prune = next_turn_candidates.len() > effective_turn_boundary_width;
    let victory_has_upside = keep_early_beam_width(best_victory.as_ref(), root_baseline);
    let run_non_prune_rollout = !*has_victory
        || (has_any_strategy_targets
            && best_victory
                .as_ref()
                .is_some_and(|v| !v.all_strategy_targets_satisfied))
        || (options.prefer_potion_buckets && victory_has_upside);

    if options.rollout_horizon_turns == 0
        || cancel.load(Ordering::Relaxed)
        || !has_time_for_rollout
        || (!needs_turn_boundary_prune && !run_non_prune_rollout)
    {
        return false;
    }

    let non_prune_rollout_cap = if options.prefer_potion_buckets {
        64usize.max(effective_turn_boundary_width)
    } else {
        32
    };
    let shortlist_cap = if needs_turn_boundary_prune {
        next_turn_candidates
            .len()
            .min(effective_turn_boundary_width * 2)
    } else {
        next_turn_candidates.len().min(non_prune_rollout_cap)
    };
    if next_turn_candidates.len() > shortlist_cap {
        BeamRetentionPolicy::prune_wave_with_rollout_into(
            arena,
            next_turn_candidates,
            shortlist_cap,
            profile.potion_free_beam_width,
            profile.potion_beam_width,
            root_baseline,
            profile.second_rank_band,
            profile.aggressive_power_commitment,
            options.prefer_potion_buckets,
            false,
            active_pool,
            shortlist_buf,
            prune_scratch,
        );
    } else {
        shortlist_buf.clear();
        shortlist_buf.extend_from_slice(next_turn_candidates);
    }
    let rollout_deadline = (profile.soft_time_budget_ms > 0)
        .then(|| started + Duration::from_millis(profile.soft_time_budget_ms));
    evaluate_turn_boundary_rollouts_into(
        turn_staging_arena,
        arena,
        shortlist_buf,
        static_ctx,
        content,
        root_baseline,
        policy,
        profile,
        options.eval_mode,
        options.rollout_horizon_turns,
        rollout_deadline,
        active_pool,
        cancel,
        rollout_workspaces,
        Some(turn_rollout_cache),
        rollout_results,
    );
    for (idx, maybe_rr) in rollout_results.drain(..).enumerate() {
        let Some(rr) = maybe_rr else { continue };
        let pid = shortlist_buf[idx];
        arena.get_mut(pid).set_rollout_score(rr.rollout_score);
        if let Some(pv) = rr.promoted_victory {
            materialize_promoted_victory(
                arena,
                pid,
                pv,
                root_baseline,
                incumbent_table,
                incumbent,
                policy,
                options.allow_paid_optional_potions,
                check_theft,
                true,
                has_victory,
                best_victory,
                hp_target_reached,
                terminal_tracker,
                completed,
            );
        }
    }
    if !*hp_target_reached
        && has_exceeded_post_victory_turn_cap(
            best_victory.as_ref(),
            next_turn_number,
            root_baseline,
            policy,
            is_elite_or_boss,
            options.allow_paid_optional_potions,
        )
    {
        *hit_post_victory_turn_cap = true;
    }
    if needs_turn_boundary_prune {
        std::mem::swap(next_turn_candidates, shortlist_buf);
        shortlist_buf.clear();
        true
    } else {
        let all_rolled_out = shortlist_buf.len() == next_turn_candidates.len();
        shortlist_buf.clear();
        options.prefer_potion_buckets && all_rolled_out
    }
}

#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn finalize_beam_pass_output(
    arena: &NodeArena,
    start_state: &BattleState,
    root_baseline: RootEvalBaseline,
    policy: &SearchPolicy,
    fixed_prefix: Option<&[PlannedAction]>,
    completed: &mut Vec<NodeId>,
    latest_completed_turn_nodes: &[NodeId],
    current_turn_frontier: &[NodeId],
    last_active_frontier: &[NodeId],
    has_victory: bool,
    mut termination: &'static str,
    was_losslessly_exhausted: bool,
    expanded_nodes: usize,
    transition_count: u64,
    choice_replay_attempts: u64,
    tt_pruned: u64,
    incumbent_pruned: u64,
    por_pruned: u64,
    packed_state_bytes_total: u64,
    early_turn_frontiers: Vec<crate::early_turn::EarlyTurnFrontierCandidate>,
    boss_tempo_prefixes: Vec<BossTempoPrefix>,
) -> BeamRunOutput {
    if !has_victory && (termination != "None" || completed.is_empty()) {
        if !latest_completed_turn_nodes.is_empty() {
            completed.extend(latest_completed_turn_nodes.iter().copied());
        } else if !current_turn_frontier.is_empty() {
            completed.extend(current_turn_frontier.iter().copied());
        } else if completed.is_empty() && !last_active_frontier.is_empty() {
            completed.extend(last_active_frontier.iter().copied());
        }
    }

    let selected = select_final_plan(
        arena,
        completed,
        &root_baseline,
        policy.potion_policy,
        Some(policy.theft_policy),
        root_baseline.potion_replacement_hp_credit,
        root_baseline.minimum_required_potion_uses,
    );

    let default_enemy_hp: i32 = start_state
        .enemy_handles
        .iter()
        .filter_map(|&eh| start_state.creature(eh))
        .filter(|c| c.is_alive())
        .map(|c| c.current_hp.max(0))
        .sum();

    let (
        best_node,
        best_turn,
        won,
        survives,
        is_event_defeat,
        death_save_use_count,
        player_hp,
        end_player_max_hp,
        all_forced_uses_satisfied,
        strategic_hp_deficit,
        net_persistent_hp_delta_centi,
        net_potion_opportunity_cost_centi,
        strategy_goal_hp_credit,
        strategy_goal_count,
        growth_rewards,
        relic_eval,
        recovered_hp,
        hp_lost,
        potion_count,
        optional_potion_count,
        optional_paid_potion_count,
        optional_potion_strategic_cost,
        optional_ambergris_count,
        used_potion_slot_mask,
        alchemize_count,
        empty_potion_slots,
        outstanding_stolen,
        forfeits_gold,
        score,
        actions,
    ) = match selected {
        Some(sel) => {
            let mut acts = fixed_prefix.map_or_else(Vec::new, |p| p.to_vec());
            acts.extend(arena.reconstruct_actions(sel.node_id));
            (
                Some(sel.node_id),
                sel.turn,
                sel.won,
                sel.survives,
                sel.is_event_defeat,
                sel.death_save_use_count,
                sel.player_hp,
                sel.end_player_max_hp,
                sel.all_forced_uses_satisfied,
                sel.strategic_hp_deficit,
                sel.net_persistent_hp_delta_centi,
                sel.net_potion_opportunity_cost_centi,
                sel.strategy_goal_hp_credit,
                sel.strategy_goal_count,
                sel.growth_rewards,
                sel.relic_eval,
                sel.recovered_hp,
                sel.hp_lost,
                sel.potion_count,
                sel.optional_potion_count,
                sel.optional_paid_potion_count,
                sel.optional_potion_strategic_cost,
                sel.optional_ambergris_count,
                sel.used_potion_slot_mask,
                sel.alchemize_count,
                sel.empty_potion_slots,
                sel.outstanding_stolen,
                sel.forfeits_gold,
                sel.score,
                acts,
            )
        }
        None => (
            None,
            start_state.player.turn_number,
            false,
            false,
            false,
            0,
            0,
            root_baseline.initial_player_max_hp,
            root_baseline.all_forced_potion_mask == 0,
            i32::MAX,
            i32::MIN / 2,
            0,
            0,
            0,
            GrowthCounters::default(),
            RelicCounterEvaluation::default(),
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            root_baseline.empty_root_potion_slots,
            0,
            false,
            0,
            Vec::new(),
        ),
    };

    let frontier_exhausted_in_event_defeat =
        !has_victory && !won && termination == "None" && is_event_defeat;
    if frontier_exhausted_in_event_defeat {
        termination = "EventDefeat";
    }

    let (has_completed_turn_ongoing, completed_ongoing_turn) = match best_node {
        Some(nid)
            if !won
                && survives
                && !is_event_defeat
                && !latest_completed_turn_nodes.is_empty() =>
        {
            (true, arena.get(nid).turn as i32)
        }
        _ => (false, -1),
    };

    let terminal_cards_hint = start_state
        .player
        .hand
        .iter()
        .filter_map(|&ch| start_state.card(ch).map(|c| c.id))
        .collect();

    let (
        explicit_potion_count,
        raw_optional_paid_potion_cost,
        enemy_hp,
        all_strategy_targets_satisfied,
    ) = match best_node {
        Some(nid) => {
            let n = arena.get(nid);
            let rare = arena.rare_eval(nid);
            (
                n.explicit_potion_count as usize,
                n.optional_paid_potion_cost as i32,
                n.fast_eval.enemy_hp,
                are_strategy_targets_satisfied_raw(&root_baseline, &n.fast_eval, &rare),
            )
        }
        None => (
            0,
            0,
            default_enemy_hp,
            !root_baseline.effective_has_growth_targets
                && root_baseline.active_relic_targets.is_empty(),
        ),
    };
    let nodes_in_arena = arena.len();

    BeamRunOutput {
        best_node,
        best_turn,
        nodes_in_arena,
        root_baseline,
        won,
        survives,
        is_event_defeat,
        death_save_use_count,
        player_hp,
        end_player_max_hp,
        enemy_hp,
        all_forced_uses_satisfied,
        all_strategy_targets_satisfied,
        strategic_hp_deficit,
        net_persistent_hp_delta_centi,
        net_potion_opportunity_cost_centi,
        strategy_goal_hp_credit,
        strategy_goal_count,
        growth_rewards,
        relic_eval,
        recovered_hp,
        hp_lost,
        potion_count,
        explicit_potion_count,
        optional_potion_count,
        optional_paid_potion_count,
        optional_potion_strategic_cost,
        raw_optional_paid_potion_cost,
        optional_ambergris_count,
        used_potion_slot_mask,
        alchemize_count,
        empty_potion_slots,
        outstanding_stolen,
        forfeits_gold,
        has_completed_turn_ongoing,
        completed_ongoing_turn,
        score,
        actions,
        expanded_nodes,
        transition_count,
        choice_replay_attempts,
        tt_pruned,
        incumbent_pruned,
        por_pruned,
        packed_state_bytes_total,
        termination,
        was_losslessly_exhausted,
        terminal_cards_hint,
        early_turn_frontiers,
        boss_tempo_prefixes,
    }
}

#[allow(clippy::too_many_arguments)]
fn run_beam_pass(
    root: &BattleState,
    content: &(dyn ContentDispatcher + Sync),
    policy: &SearchPolicy,
    profile: &SolverSearchProfile,
    cert: &ClosureCertificate,
    incumbent_table: &PrimaryIncumbentTable,
    incumbent: &mut FrozenIncumbentBound,
    pool: Option<&rayon::ThreadPool>,
    cancel: &AtomicBool,
    abort: &SearchAbortSlot,
    progress_cb: Option<&(dyn Fn(&SearchProgress) + Sync)>,
    options: PassSearchOptions,
    fixed_prefix: Option<&[PlannedAction]>,
) -> Result<BeamRunOutput, KernelError> {
    let dop = policy
        .max_degree_of_parallelism
        .clamp(1, crate::eval::weights::MAXIMUM_SEARCH_MAX_DEGREE_OF_PARALLELISM);
    let active_pool = if dop > 1 { pool } else { None };
    let worker_count = if active_pool.is_some() { dop } else { 1 };
    let mut resources = PassResources::acquire(
        &root.static_ctx,
        worker_count,
        policy.transposition_entry_limit,
    );
    run_beam_pass_with_resources(
        root,
        content,
        policy,
        profile,
        cert,
        incumbent_table,
        incumbent,
        pool,
        cancel,
        abort,
        progress_cb,
        options,
        fixed_prefix,
        &mut resources,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_beam_pass_with_resources(
    root: &BattleState,
    content: &(dyn ContentDispatcher + Sync),
    policy: &SearchPolicy,
    profile: &SolverSearchProfile,
    cert: &ClosureCertificate,
    incumbent_table: &PrimaryIncumbentTable,
    incumbent: &mut FrozenIncumbentBound,
    pool: Option<&rayon::ThreadPool>,
    cancel: &AtomicBool,
    abort: &SearchAbortSlot,
    progress_cb: Option<&(dyn Fn(&SearchProgress) + Sync)>,
    options: PassSearchOptions,
    fixed_prefix: Option<&[PlannedAction]>,
    resources: &mut PassResources,
) -> Result<BeamRunOutput, KernelError> {
    let prev_allow_death_save_rescue = resources.prune_scratch.allow_death_save_rescue;
    let prev_relax_zero_potion_reserve = resources.prune_scratch.relax_zero_potion_reserve;
    let prev_min_zero_potion_reserve_percent =
        resources.prune_scratch.min_zero_potion_reserve_percent;
    let prev_turn_layer = resources.prune_scratch.turn_layer;
    resources.prune_scratch.allow_death_save_rescue = options.allow_death_save_rescue;
    resources.prune_scratch.relax_zero_potion_reserve = options.relax_zero_potion_reserve;
    resources.prune_scratch.min_zero_potion_reserve_percent =
        options.min_zero_potion_reserve_percent;
    resources.prune_scratch.turn_layer = 0;

    let dop = policy
        .max_degree_of_parallelism
        .clamp(1, crate::eval::weights::MAXIMUM_SEARCH_MAX_DEGREE_OF_PARALLELISM);
    let res = if dop > 1 {
        if let Some(p) = pool {
            p.install(|| {
                run_beam_pass_with_resources_inner(
                    root,
                    content,
                    policy,
                    profile,
                    cert,
                    incumbent_table,
                    incumbent,
                    pool,
                    cancel,
                    abort,
                    progress_cb,
                    options,
                    fixed_prefix,
                    resources,
                )
            })
        } else {
            run_beam_pass_with_resources_inner(
                root,
                content,
                policy,
                profile,
                cert,
                incumbent_table,
                incumbent,
                pool,
                cancel,
                abort,
                progress_cb,
                options,
                fixed_prefix,
                resources,
            )
        }
    } else {
        run_beam_pass_with_resources_inner(
            root,
            content,
            policy,
            profile,
            cert,
            incumbent_table,
            incumbent,
            pool,
            cancel,
            abort,
            progress_cb,
            options,
            fixed_prefix,
            resources,
        )
    };
    resources.prune_scratch.allow_death_save_rescue = prev_allow_death_save_rescue;
    resources.prune_scratch.relax_zero_potion_reserve = prev_relax_zero_potion_reserve;
    resources.prune_scratch.min_zero_potion_reserve_percent =
        prev_min_zero_potion_reserve_percent;
    resources.prune_scratch.turn_layer = prev_turn_layer;
    res
}

#[allow(clippy::too_many_arguments)]
fn run_beam_pass_with_resources_inner(
    root: &BattleState,
    content: &(dyn ContentDispatcher + Sync),
    policy: &SearchPolicy,
    profile: &SolverSearchProfile,
    cert: &ClosureCertificate,
    incumbent_table: &PrimaryIncumbentTable,
    incumbent: &mut FrozenIncumbentBound,
    pool: Option<&rayon::ThreadPool>,
    cancel: &AtomicBool,
    abort: &SearchAbortSlot,
    progress_cb: Option<&(dyn Fn(&SearchProgress) + Sync)>,
    options: PassSearchOptions,
    fixed_prefix: Option<&[PlannedAction]>,
    resources: &mut PassResources,
) -> Result<BeamRunOutput, KernelError> {
    let started = Instant::now();
    let root_baseline = RootEvalBaseline::from_state_with_policy(root, content, policy);
    let sensitivity = CombatOrderSensitivity::analyze(root, content);
    let dop = policy
        .max_degree_of_parallelism
        .clamp(1, crate::eval::weights::MAXIMUM_SEARCH_MAX_DEGREE_OF_PARALLELISM);
    let active_pool = if dop > 1 { pool } else { None };
    let worker_count = if active_pool.is_some() { dop } else { 1 };
    let is_boss = root.static_ctx.room_type == RoomType::Boss;
    let is_elite_or_boss = matches!(root.static_ctx.room_type, RoomType::Elite | RoomType::Boss);
    let has_any_strategy_targets = root_baseline.effective_has_growth_targets
        || !root_baseline.active_relic_targets.is_empty();
    let has_relic_targets = !root_baseline.active_relic_targets.is_empty();

    resources.reset_for_pass();
    let PassResources {
        scratches,
        parent_states,
        enumerated_tasks,
        rollout_workspaces,
        node_arena,
        prune_scratch,
        flat_tasks,
        deferred_batches,
        probe_jobs,
        step_arena,
        prev_worker_slabs,
        turn_staging_arena,
        tt,
        choice_probe_cache,
        turn_rollout_cache,
        stand_pat_seen,
        sort_list,
        parent_slices,
        active_parent_slices,
        tt_group_accepted,
        tt_accepted_epoch,
        tt_cur_epoch,
        tt_shard_buckets,
        tt_accepted_flags,
        tt_flat_refs,
        accepted_mid_turn,
        mid_turn_ords,
        surviving_ords,
        surviving_commit_order,
        rollout_results,
    } = resources;
    debug_assert!(scratches.len() >= worker_count && parent_states.len() >= worker_count);
    let use_l1_probe = dop > 1 && active_pool.is_some();
    for s in scratches.iter_mut() {
        s.choice_probe_ws.l1.use_l1 = use_l1_probe;
    }

    let arena = node_arena;
    let mut current_turn_frontier: Vec<NodeId> = Vec::new();
    let mut completed: Vec<NodeId> = Vec::new();
    let mut terminal_tracker = crate::eval::FinalPlanTracker::new(
        &root_baseline,
        policy.potion_policy,
        Some(policy.theft_policy),
        root_baseline.minimum_required_potion_uses,
    );
    let (
        start_state,
        root_id,
        mut best_seen_max_dark_evoke,
        mut best_seen_poison_and_doom,
        mut best_seen_board_investment,
        mut packed_state_bytes_total,
    ) = init_beam_pass_root(
        root,
        content,
        policy,
        profile,
        options.eval_mode,
        fixed_prefix,
        &root_baseline,
        &mut scratches[0].pack_buf,
        arena,
        step_arena,
        &mut completed,
        &mut current_turn_frontier,
        &mut terminal_tracker,
    )?;

    let mut expanded_nodes = 0usize;
    let mut transition_count = 0u64;
    let mut choice_replay_attempts = 0u64;
    let mut incumbent_pruned = 0u64;
    let mut por_pruned = 0u64;
    let mut branch_truncations = 0u64;
    let mut termination: &'static str = "None";
    let mut early_turn_frontiers = Vec::new();
    let mut boss_tempo_prefixes = Vec::new();
    let mut latest_completed_turn_nodes: Vec<NodeId> = Vec::new();
    let mut last_active_frontier: Vec<NodeId> = Vec::new();
    let mut active_frontier: Vec<NodeId> = Vec::new();
    let mut next_step_candidates: Vec<NodeId> = Vec::new();
    let mut next_turn_candidates: Vec<NodeId> = Vec::new();
    let mut turn_setup_ended_turn: Vec<NodeId> = Vec::new();
    let mut shortlist_buf: Vec<NodeId> = Vec::new();
    let mut sp_mid_unused: Vec<NodeId> = Vec::new();

    let check_theft = policy.theft_policy == SolverTheftPolicy::PreserveResources;
    let mut has_victory = false;
    let mut best_victory: Option<BestVictoryState> = None;
    for &nid in &completed {
        let n = arena.get(nid).clone();
        if n.fast_eval.won() {
            let rare = arena.rare_eval(nid);
            record_victory_and_tighten(
                &root_baseline,
                incumbent_table,
                incumbent,
                policy,
                &n.fast_eval,
                &rare,
                n.turn as i32,
                n.potion_count as u16,
                n.explicit_potion_count as u16,
                n.optional_paid_potion_count as u16,
                n.potion_strategic_cost as i32,
                n.optional_paid_potion_cost as i32,
                n.forced_used_mask,
                n.beam_rank_score_or_base().round() as i64,
                &mut has_victory,
                &mut best_victory,
            );
        }
    }
    let mut last_progress_ms: Option<u64> = None;
    let root_fast_eval = arena.get(root_id).fast_eval;
    let mut best_seen_enemy_hp = root_fast_eval.enemy_hp;
    let mut best_seen_alive_enemies =
        (root_fast_eval.alive_enemy_count as i32) + (root_fast_eval.reviving_enemy_count as i32);
    let mut best_seen_strategy_goal_hp_credit = if has_any_strategy_targets {
        root_fast_eval.strategy_goal_hp_credit as i32
    } else {
        0
    };
    let mut best_seen_strategy_goal_count = if has_any_strategy_targets {
        root_fast_eval.strategy_goal_count as i32
    } else {
        0
    };
    let mut seen_relic_satisfied_mask = if has_relic_targets {
        arena.rare_eval(root_id).relic_eval.satisfied_mask
    } else {
        0
    };
    let mut turns_without_progress = 0i32;
    let mut turns_since_enemy_hp_decreased = 0i32;

    const MAX_ACTIONS_PER_TURN: usize = 64;
    let max_turns = policy.max_turns.max(1);
    let beam_width = profile.beam_width.clamp(1, MAX_BEAM_WIDTH);
    let early_turn_boundary_width = ((beam_width * 3) / 2).min(MAX_BEAM_WIDTH).max(beam_width);
    let early_mid_turn_width = ((beam_width * 5) / 4).min(MAX_BEAM_WIDTH).max(beam_width);
    let turn_boundary_rollouts_enabled = options.rollout_horizon_turns > 0;
    let mut any_lossy_truncation = false;
    let mut any_turn_hit_per_turn_cap = false;
    let mut hit_max_search_turns = false;
    let mut hit_post_victory_turn_cap = false;
    let mut hit_progress_stall = false;
    let mut any_stop_target_reached = false;

    if !arena.get(root_id).is_terminal() && start_state.pending.is_some() {
        let prefix_len = fixed_prefix.map_or(0, |p| p.len() as u16);
        let rn = arena.get(root_id);
        expand_depth0_turn_setup(
            &start_state,
            root_id,
            prefix_len,
            rn.potion_count as u16,
            rn.optional_paid_potion_count as u16,
            rn.used_potion_slot_mask,
            rn.potion_strategic_cost as i32,
            rn.optional_paid_potion_cost as i32,
            rn.ambergris_count as u16,
            rn.forced_used_mask,
            rn.forced_ambergris_count as u16,
            started,
            beam_width,
            check_theft,
            dop,
            active_pool,
            cancel,
            abort,
            content,
            &root_baseline,
            policy,
            profile,
            cert,
            incumbent_table,
            incumbent,
            &sensitivity,
            &options,
            scratches,
            deferred_batches,
            probe_jobs,
            rollout_workspaces,
            rollout_results,
            turn_rollout_cache,
            choice_probe_cache,
            tt,
            arena,
            step_arena,
            turn_staging_arena,
            prune_scratch,
            active_parent_slices,
            tt_group_accepted,
            tt_accepted_epoch,
            tt_cur_epoch,
            tt_shard_buckets,
            tt_accepted_flags,
            tt_flat_refs,
            parent_slices,
            sort_list,
            &mut completed,
            &mut terminal_tracker,
            &mut next_step_candidates,
            &mut turn_setup_ended_turn,
            &mut shortlist_buf,
            &mut current_turn_frontier,
            &mut transition_count,
            &mut choice_replay_attempts,
            &mut incumbent_pruned,
            &mut por_pruned,
            &mut branch_truncations,
            &mut any_lossy_truncation,
            &mut packed_state_bytes_total,
            &mut has_victory,
            &mut best_victory,
            &mut termination,
        )?;
        if termination == "HpTarget" {
            any_stop_target_reached = true;
        }
    }

    for turn_layer in 0..max_turns {
        if termination == "HpTarget" {
            any_stop_target_reached = true;
            break;
        }
        if current_turn_frontier.is_empty() && (turn_layer > 0 || turn_setup_ended_turn.is_empty()) {
            break;
        }
        if has_exceeded_post_victory_turn_cap(
            best_victory.as_ref(),
            root_baseline.start_turn_number + turn_layer,
            &root_baseline,
            policy,
            is_elite_or_boss,
            options.allow_paid_optional_potions,
        ) {
            hit_post_victory_turn_cap = true;
            break;
        }
        if let Some(err) = abort.clone_error() {
            return Err(err);
        }
        if cancel.load(Ordering::Relaxed) {
            termination = "Cancelled";
            break;
        }
        if expanded_nodes >= profile.max_expanded_nodes {
            termination = BeamWidthPortfolio::NODE_LIMIT_TERMINATION;
            break;
        }
        if profile.soft_time_budget_ms > 0
            && (started.elapsed().as_millis() as u64) >= profile.soft_time_budget_ms
        {
            termination = "TimeLimit";
            break;
        }

        prune_scratch.turn_layer = turn_layer;
        if turn_layer > 0 {
            turn_staging_arena.clear();
        }
        active_frontier.clear();
        active_frontier.append(&mut current_turn_frontier);
        next_turn_candidates.clear();
        if turn_layer == 0 && !turn_setup_ended_turn.is_empty() {
            next_turn_candidates.append(&mut turn_setup_ended_turn);
        }
        let remaining_pass_nodes = profile.max_expanded_nodes.saturating_sub(expanded_nodes);
        let is_boss_deep_rescue_pass = options.prefer_potion_buckets
            || options.allow_death_save_rescue
            || matches!(
                options.eval_mode,
                EvalMode::Offense | EvalMode::DesperateRescue
            );
        let base_turn_share = if is_boss {
            if is_boss_deep_rescue_pass {
                (beam_width * 32).max(profile.max_expanded_nodes / 4)
            } else {
                (beam_width * 24).max(profile.max_expanded_nodes / 8)
            }
        } else {
            (beam_width * 40).max(profile.max_expanded_nodes / 4)
        };
        let keep_post_victory_turn_cap =
            has_victory && keep_early_beam_width(best_victory.as_ref(), &root_baseline);
        let per_turn_cap = if has_victory && !keep_post_victory_turn_cap {
            remaining_pass_nodes
        } else {
            remaining_pass_nodes.min(base_turn_share).max(
                crate::eval::weights::MINIMUM_TURN_LAYER_EXPANDED_NODES
                    .min(remaining_pass_nodes.max(1)),
            )
        };
        let mut expanded_this_turn = 0usize;
        let mut last_turn_hit_per_turn_cap = false;
        let mut aborted_limit = false;
        let mut hp_target_reached = false;
        let mut frontier_in_worker_slabs = false;

        for _action_depth in 0..=MAX_ACTIONS_PER_TURN {
            if active_frontier.is_empty() {
                break;
            }
            if let Some(err) = abort.clone_error() {
                return Err(err);
            }
            if cancel.load(Ordering::Relaxed) {
                termination = "Cancelled";
                aborted_limit = true;
                break;
            }
            if expanded_nodes >= profile.max_expanded_nodes {
                termination = BeamWidthPortfolio::NODE_LIMIT_TERMINATION;
                aborted_limit = true;
                break;
            }
            if expanded_this_turn >= per_turn_cap {
                last_turn_hit_per_turn_cap = true;
                any_turn_hit_per_turn_cap = true;
                break;
            }
            if profile.soft_time_budget_ms > 0
                && (started.elapsed().as_millis() as u64) >= profile.soft_time_budget_ms
            {
                termination = "TimeLimit";
                aborted_limit = true;
                break;
            }

            let allowed = (profile.max_expanded_nodes - expanded_nodes)
                .min(per_turn_cap - expanded_this_turn);
            let wave_count = active_frontier.len().min(allowed);
            if wave_count == 0 {
                break;
            }
            if wave_count < active_frontier.len() {
                any_lossy_truncation = true;
            }

            let parent_slice = &active_frontier[..wave_count];
            expanded_nodes += wave_count;
            expanded_this_turn += wave_count;

            let frozen_incumbent = *incumbent;
            let parent_source = if frontier_in_worker_slabs {
                ParentStateSource::WorkerSlabs(prev_worker_slabs.as_slice())
            } else {
                ParentStateSource::Main(step_arena)
            };
            let commit_slice_count = expand_wave_tasks(
                parent_slice,
                dop,
                active_pool,
                &root.static_ctx,
                arena,
                parent_source,
                content,
                &root_baseline,
                policy,
                profile,
                cert,
                &frozen_incumbent,
                tt,
                choice_probe_cache,
                &sensitivity,
                options.allow_paid_optional_potions,
                turn_boundary_rollouts_enabled,
                options.eval_mode,
                cancel,
                abort,
                scratches,
                parent_states,
                enumerated_tasks,
                flat_tasks,
                deferred_batches,
                probe_jobs,
                parent_slices,
            );
            if let Some(err) = abort.clone_error() {
                return Err(err);
            }

            next_step_candidates.clear();
            accepted_mid_turn.clear();
            commit_wave_candidates(
                scratches,
                tt,
                arena,
                turn_staging_arena,
                &root_baseline,
                policy,
                incumbent_table,
                incumbent,
                check_theft,
                &options,
                dop,
                active_pool,
                active_parent_slices,
                tt_group_accepted,
                tt_accepted_epoch,
                tt_cur_epoch,
                tt_shard_buckets,
                tt_accepted_flags,
                tt_flat_refs,
                &mut completed,
                &mut terminal_tracker,
                &mut next_step_candidates,
                &mut next_turn_candidates,
                Some(accepted_mid_turn),
                &mut transition_count,
                &mut choice_replay_attempts,
                &mut incumbent_pruned,
                &mut por_pruned,
                &mut branch_truncations,
                &mut packed_state_bytes_total,
                &mut has_victory,
                &mut best_victory,
                &mut hp_target_reached,
                commit_slice_count,
                true,
                parent_slices,
                sort_list,
            );

            if hp_target_reached {
                any_stop_target_reached = true;
                termination = "HpTarget";
                break;
            }

            if accepted_mid_turn.is_empty() {
                active_frontier.clear();
                if frontier_in_worker_slabs {
                    for slab in prev_worker_slabs.iter_mut() {
                        slab.clear();
                    }
                    frontier_in_worker_slabs = false;
                }
            } else {
                let effective_mid_turn_width = if turn_layer <= 2
                    && keep_early_beam_width(best_victory.as_ref(), &root_baseline)
                {
                    early_mid_turn_width
                } else {
                    beam_width
                };
                mid_turn_ords.clear();
                mid_turn_ords.extend((0..accepted_mid_turn.len() as u32).map(NodeId));
                let cand_source = WorkerCandidateSource {
                    scratches,
                    accepted: accepted_mid_turn,
                };
                BeamRetentionPolicy::prune_with_source_into(
                    &cand_source,
                    mid_turn_ords,
                    effective_mid_turn_width,
                    profile.potion_free_beam_width,
                    profile.potion_beam_width,
                    &root_baseline,
                    profile.second_rank_band,
                    profile.aggressive_power_commitment,
                    options.prefer_potion_buckets,
                    false,
                    false,
                    active_pool,
                    surviving_ords,
                    prune_scratch,
                );
                if surviving_ords.len() < accepted_mid_turn.len()
                    || accepted_mid_turn.len() > effective_mid_turn_width
                {
                    any_lossy_truncation = true;
                }
                active_frontier.resize(surviving_ords.len(), NodeId(0));
                surviving_commit_order.clear();
                surviving_commit_order.extend(0..surviving_ords.len());
                surviving_commit_order.sort_unstable_by_key(|&k| surviving_ords[k].0);
                let surv_len = surviving_commit_order.len();
                for pos in 0..surv_len {
                    #[cfg(target_arch = "x86_64")]
                    if surv_len >= 16 && pos + 2 < surv_len {
                        let k2 = surviving_commit_order[pos + 2];
                        let acc2 = accepted_mid_turn[surviving_ords[k2].0 as usize];
                        let wo2 = &scratches[acc2.w_idx as usize];
                        let cand2 = &wo2.candidates[acc2.c_idx as usize];
                        unsafe {
                            std::arch::x86_64::_mm_prefetch(
                                (cand2 as *const WorkerCandidate).cast::<i8>(),
                                std::arch::x86_64::_MM_HINT_T0,
                            );
                        }
                    }
                    let k = surviving_commit_order[pos];
                    let ord = surviving_ords[k].0 as usize;
                    let acc = accepted_mid_turn[ord];
                    let (w_idx, c_idx) = (acc.w_idx as usize, acc.c_idx as usize);
                    let wo = &scratches[w_idx];
                    let cand = &wo.candidates[c_idx];
                    debug_assert!(w_idx < 65536 && cand.state_slab_idx < 65536);
                    packed_state_bytes_total += u64::from(cand.state_len);
                    let state_handle = PackedHandle {
                        slab: ((w_idx as u32) << 16) | (cand.state_slab_idx & 0xFFFF),
                        offset: cand.state_off,
                        len: cand.state_len,
                    };
                    let (choices_offset, choices_len) = arena.choices.extend_from_arena_slice(
                        &wo.staged_choices,
                        cand.choices_offset,
                        cand.choices_len,
                    );
                    let nid = push_worker_candidate_node(
                        arena,
                        cand,
                        state_handle,
                        choices_offset,
                        choices_len,
                    );
                    active_frontier[k] = nid;
                }
                for (s, prev) in scratches.iter_mut().zip(prev_worker_slabs.iter_mut()) {
                    std::mem::swap(&mut s.slab, prev);
                    s.slab.clear();
                }
                frontier_in_worker_slabs = true;
            }

            if let Some(cb) = progress_cb {
                emit_search_progress(
                    cb,
                    started,
                    &mut last_progress_ms,
                    expanded_nodes,
                    transition_count,
                    choice_replay_attempts,
                    root_baseline.start_turn_number + turn_layer,
                    has_victory,
                    best_victory.map(|v| v.raw_won_hp_lost),
                );
            }
        }

        if !active_frontier.is_empty() && !hp_target_reached && termination == "None" {
            any_lossy_truncation = true;
        }

        let mut effective_turn_boundary_width = if turn_layer <= 2
            && keep_early_beam_width(best_victory.as_ref(), &root_baseline)
        {
            early_turn_boundary_width
        } else {
            beam_width
        };
        let is_time_expired = termination == "TimeLimit";
        let emergency_turn0_stand_pat = !has_victory
            && latest_completed_turn_nodes.is_empty()
            && turn_layer == 0
            && !active_frontier.is_empty()
            && !cancel.load(Ordering::Relaxed);
        let need_stand_pat = !hp_target_reached
            && !cancel.load(Ordering::Relaxed)
            && !active_frontier.is_empty()
            && ((next_turn_candidates.len() < effective_turn_boundary_width && !is_time_expired)
                || emergency_turn0_stand_pat);
        if need_stand_pat {
            let stand_pat_limit = if is_time_expired { 4 } else { 16 };
            flush_stand_pat_end_turn(
                &active_frontier,
                stand_pat_limit,
                frontier_in_worker_slabs,
                check_theft,
                dop,
                active_pool,
                cancel,
                abort,
                &root.static_ctx,
                content,
                &root_baseline,
                policy,
                profile,
                cert,
                incumbent_table,
                incumbent,
                &sensitivity,
                &options,
                scratches,
                parent_states,
                deferred_batches,
                probe_jobs,
                stand_pat_seen,
                choice_probe_cache,
                tt,
                arena,
                step_arena,
                prev_worker_slabs,
                turn_staging_arena,
                active_parent_slices,
                tt_group_accepted,
                tt_accepted_epoch,
                tt_cur_epoch,
                tt_shard_buckets,
                tt_accepted_flags,
                tt_flat_refs,
                parent_slices,
                sort_list,
                &mut completed,
                &mut terminal_tracker,
                &mut sp_mid_unused,
                &mut next_turn_candidates,
                &mut transition_count,
                &mut choice_replay_attempts,
                &mut incumbent_pruned,
                &mut por_pruned,
                &mut branch_truncations,
                &mut packed_state_bytes_total,
                &mut has_victory,
                &mut best_victory,
                &mut hp_target_reached,
                &mut termination,
            )?;
            if hp_target_reached {
                any_stop_target_reached = true;
            }
        }

        if !active_frontier.is_empty() {
            std::mem::swap(&mut last_active_frontier, &mut active_frontier);
            active_frontier.clear();
        }

        if !next_turn_candidates.is_empty()
            && (!aborted_limit || latest_completed_turn_nodes.is_empty())
        {
            latest_completed_turn_nodes.clear();
            latest_completed_turn_nodes.extend(
                next_turn_candidates
                    .iter()
                    .copied()
                    .filter(|&nid| {
                        let n = arena.get(nid);
                        !n.fast_eval.player_dead() && n.fast_eval.projected_player_hp > 0
                    }),
            );
        }

        let completed_turns = turn_layer + 1;
        if options.early_turn_scout_depth > 0
            && completed_turns <= options.early_turn_scout_depth
            && collect_early_turn_scout_layer(
                arena,
                &next_turn_candidates,
                root_baseline.start_turn_number,
                completed_turns,
                options.early_turn_scout_depth,
                &mut early_turn_frontiers,
                &mut termination,
            )
        {
            break;
        }

        if options.boss_tempo_scout_turns > 0
            && completed_turns >= options.boss_tempo_scout_turns
        {
            collect_boss_tempo_scout_prefixes(
                arena,
                &next_turn_candidates,
                &root_baseline,
                fixed_prefix,
                &mut boss_tempo_prefixes,
                &mut termination,
            );
            break;
        }

        if hp_target_reached || aborted_limit {
            break;
        }

        if next_turn_candidates.is_empty() {
            if last_turn_hit_per_turn_cap && !has_victory && termination == "None" {
                termination = BeamWidthPortfolio::NODE_LIMIT_TERMINATION;
            }
            break;
        }
        let next_turn_number = root_baseline.start_turn_number + turn_layer + 1;
        if has_exceeded_post_victory_turn_cap(
            best_victory.as_ref(),
            next_turn_number,
            &root_baseline,
            policy,
            is_elite_or_boss,
            options.allow_paid_optional_potions,
        ) {
            hit_post_victory_turn_cap = true;
            break;
        }

        effective_turn_boundary_width = if turn_layer <= 2
            && keep_early_beam_width(best_victory.as_ref(), &root_baseline)
        {
            early_turn_boundary_width
        } else {
            beam_width
        };

        let ran_rollout = run_turn_boundary_rollouts_and_promote(
            started,
            effective_turn_boundary_width,
            next_turn_number,
            is_elite_or_boss,
            check_theft,
            active_pool,
            cancel,
            &root.static_ctx,
            content,
            &root_baseline,
            policy,
            profile,
            incumbent_table,
            incumbent,
            &options,
            arena,
            turn_staging_arena,
            prune_scratch,
            &mut next_turn_candidates,
            &mut shortlist_buf,
            rollout_workspaces,
            turn_rollout_cache,
            rollout_results,
            &mut has_victory,
            &mut best_victory,
            &mut hp_target_reached,
            &mut hit_post_victory_turn_cap,
            &mut completed,
            &mut terminal_tracker,
        );

        if hp_target_reached {
            any_stop_target_reached = true;
            termination = "HpTarget";
            break;
        }
        if hit_post_victory_turn_cap {
            break;
        }

        effective_turn_boundary_width = if turn_layer <= 2
            && keep_early_beam_width(best_victory.as_ref(), &root_baseline)
        {
            early_turn_boundary_width
        } else {
            beam_width
        };

        BeamRetentionPolicy::prune_wave_with_rollout_into(
            arena,
            &next_turn_candidates,
            effective_turn_boundary_width,
            profile.potion_free_beam_width,
            profile.potion_beam_width,
            &root_baseline,
            profile.second_rank_band,
            profile.aggressive_power_commitment,
            options.prefer_potion_buckets,
            ran_rollout,
            active_pool,
            &mut current_turn_frontier,
            prune_scratch,
        );
        if current_turn_frontier.len() < next_turn_candidates.len()
            || next_turn_candidates.len() > effective_turn_boundary_width
        {
            any_lossy_truncation = true;
        }

        let mut layer_progressed = false;
        let mut enemy_hp_decreased = false;
        for &nid in &current_turn_frontier {
            let n = arena.get(nid);
            let f = &n.fast_eval;
            let rem_enemies = (f.alive_enemy_count as i32) + (f.reviving_enemy_count as i32);
            if f.enemy_hp < best_seen_enemy_hp {
                best_seen_enemy_hp = f.enemy_hp;
                layer_progressed = true;
                enemy_hp_decreased = true;
            }
            if rem_enemies < best_seen_alive_enemies {
                best_seen_alive_enemies = rem_enemies;
                layer_progressed = true;
            }
            if n.has_full_eval() {
                if (n.max_dark_evoke as i32) > best_seen_max_dark_evoke {
                    best_seen_max_dark_evoke = n.max_dark_evoke as i32;
                    layer_progressed = true;
                }
                if (n.total_poison_and_doom as i32) > best_seen_poison_and_doom {
                    best_seen_poison_and_doom = n.total_poison_and_doom as i32;
                    layer_progressed = true;
                }
                if (n.board_investment as i32) > best_seen_board_investment {
                    best_seen_board_investment = n.board_investment as i32;
                    layer_progressed = true;
                }
            }
            if has_any_strategy_targets {
                let goal_credit = f.strategy_goal_hp_credit as i32;
                if goal_credit > best_seen_strategy_goal_hp_credit {
                    best_seen_strategy_goal_hp_credit = goal_credit;
                    layer_progressed = true;
                }
                let goal_count = f.strategy_goal_count as i32;
                if goal_count > best_seen_strategy_goal_count {
                    best_seen_strategy_goal_count = goal_count;
                    layer_progressed = true;
                }
                if has_relic_targets {
                    let sat_mask = arena.rare_eval(nid).relic_eval.satisfied_mask;
                    if (sat_mask & !seen_relic_satisfied_mask) != 0 {
                        seen_relic_satisfied_mask |= sat_mask;
                        layer_progressed = true;
                    }
                }
            }
        }
        turns_without_progress = if layer_progressed { 0 } else { turns_without_progress + 1 };
        turns_since_enemy_hp_decreased = if enemy_hp_decreased {
            0
        } else {
            turns_since_enemy_hp_decreased + 1
        };
        if turns_without_progress >= crate::eval::weights::SETUP_VALUE_HORIZON_TURNS {
            hit_progress_stall = true;
            if !has_victory {
                termination = "TurnLimit";
            }
            break;
        }
        let boss_extended_turn_stalled = is_boss
            && turn_layer + 1 >= 28
            && turns_since_enemy_hp_decreased > 2;
        if boss_extended_turn_stalled
            || (turn_layer + 1 == max_turns && !current_turn_frontier.is_empty())
        {
            hit_max_search_turns = true;
            if !has_victory && termination == "None" {
                termination = "TurnLimit";
            }
            break;
        }
        step_arena.clear();
        for &nid in &current_turn_frontier {
            let tmp_h = arena.get(nid).state_handle;
            if tmp_h.len > 0 {
                let bytes = turn_staging_arena.get(tmp_h);
                arena.get_mut(nid).state_handle = step_arena.commit_slice(bytes);
            }
        }
        turn_staging_arena.clear();
        for slab in prev_worker_slabs.iter_mut() {
            slab.clear();
        }
        tt.reset_generation();
        stand_pat_seen.clear();
    }

    let was_losslessly_exhausted = !any_lossy_truncation
        && !any_turn_hit_per_turn_cap
        && !hit_max_search_turns
        && !hit_post_victory_turn_cap
        && !hit_progress_stall
        && !any_stop_target_reached
        && termination == "None"
        && branch_truncations == 0;

    Ok(finalize_beam_pass_output(
        arena,
        &start_state,
        root_baseline,
        policy,
        fixed_prefix,
        &mut completed,
        &latest_completed_turn_nodes,
        &current_turn_frontier,
        &last_active_frontier,
        has_victory,
        termination,
        was_losslessly_exhausted,
        expanded_nodes,
        transition_count,
        choice_replay_attempts,
        tt.branches_pruned,
        incumbent_pruned,
        por_pruned,
        packed_state_bytes_total,
        early_turn_frontiers,
        boss_tempo_prefixes,
    ))
}

#[cold]
#[inline(never)]
fn run_novelty_pass(
    root: &BattleState,
    content: &(dyn ContentDispatcher + Sync),
    policy: &SearchPolicy,
    profile: &SolverSearchProfile,
    cert: &ClosureCertificate,
    _incumbent_table: &PrimaryIncumbentTable,
    cancel: &AtomicBool,
    abort: &SearchAbortSlot,
    options: &NoveltySearchOptions,
) -> Result<BeamRunOutput, KernelError> {
    let started = Instant::now();
    let root_baseline = RootEvalBaseline::from_state_with_policy(root, content, policy);
    let sensitivity = CombatOrderSensitivity::analyze(root, content);
    let mut arena = NodeArena::default();
    let mut main_arena = MainStateArena::default();
    let mut scratch = WorkerScratch::new(root.static_ctx.clone());
    let mut parent_state = empty_scratch_state(root.static_ctx.clone());
    let tt = ShardedParetoTT::new(policy.transposition_entry_limit.max(65_536));
    let mut probe_cache = ChoiceProbeCache::new();
    let frozen_incumbent = FrozenIncumbentBound::default();

    pack_state(root, &mut scratch.pack_buf);
    let root_hash = hash_canonical_key(root);
    let root_fast = evaluate_fast(root, content, &root_baseline, None);
    let root_full = evaluate_full(root, content, &root_fast, &root_baseline);
    let root_rare_idx = arena.push_rare_eval(RareNodeEval {
        growth_rewards: root_fast.growth_rewards,
        relic_eval: root_fast.relic_eval,
    });
    let root_terminal =
        !root.is_in_progress || root_fast.won || root_fast.player_dead || root_fast.is_event_defeat;
    let mut root_node = SearchNode {
        state_handle: main_arena.commit_slice(&scratch.pack_buf),
        state_hash: root_hash,
        beam_rank_score: root_fast.base_score,
        turn: root
            .player
            .turn_number
            .clamp(i16::MIN as i32, i16::MAX as i32) as i16,
        node_flags: if root_terminal {
            NODE_FLAG_IS_TERMINAL
        } else {
            0
        },
        fast_eval: StoredFastEval::from(&root_fast),
        rare_eval_idx: root_rare_idx,
        ..Default::default()
    };
    root_node.set_full_eval(&root_full.to_stored());
    let root_id = arena.push_node(root_node);

    let mut open = BfwsBoundedOpen::new(options.max_open);
    let mut novelty = BfwsPackedNovelty::new(options.width, options.max_novelty_entries);
    let mut escapes: Vec<BfwsEscapeBudget> = Vec::new();
    let mut facts_buf: Vec<BfwsFact> = Vec::with_capacity(64);
    let mut completed = Vec::new();
    let mut sequence = 0u64;

    capture_novelty_facts(root, &root_fast, &root_full, &mut facts_buf);
    let w0 = novelty.evaluate((root_fast.enemy_hp / 10, 0), &facts_buf);
    if arena.get(root_id).is_terminal() {
        completed.push(root_id);
    } else {
        let esc_idx = if options.familiar_allowance > 0 {
            let i = escapes.len();
            escapes.push(BfwsEscapeBudget::new(options.familiar_allowance));
            Some(i)
        } else {
            None
        };
        open.enqueue(
            NoveltyOpenNode {
                node_id: root_id,
                escape_idx: esc_idx,
            },
            w0 as i32,
            root_fast.base_score,
            sequence,
        );
        sequence += 1;
    }

    let mut expanded_nodes = 0usize;
    let mut por_pruned = 0u64;
    let mut termination = "OpenExhausted";

    while let Some(item) = open.dequeue() {
        if let Some(err) = abort.clone_error() {
            return Err(err);
        }
        if cancel.load(Ordering::Relaxed) {
            termination = "Cancelled";
            break;
        }
        if expanded_nodes >= profile.max_expanded_nodes {
            termination = BeamWidthPortfolio::NODE_LIMIT_TERMINATION;
            break;
        }
        if profile.soft_time_budget_ms > 0
            && (started.elapsed().as_millis() as u64) >= profile.soft_time_budget_ms
        {
            termination = "TimeLimit";
            break;
        }

        let pnode = arena.get(item.node_id).clone();
        if pnode.action_count as usize >= options.max_actions
            || (pnode.turn as i32) >= root_baseline.start_turn_number + options.max_turns
        {
            continue;
        }

        expanded_nodes += 1;
        let pbytes = main_arena.get(pnode.state_handle);
        // SAFETY: `pbytes` was committed from `pack_state` output in this process.
        if unsafe { unpack_state(pbytes, &root.static_ctx, &mut parent_state) }.is_err() {
            continue;
        }
        let p_has_la = pnode.has_choice_lookahead();
        let p_la_score = pnode.choice_lookahead_score();
        let pfast = evaluate_fast(&parent_state, content, &root_baseline, None);
        let pfull = evaluate_full(&parent_state, content, &pfast, &root_baseline);
        capture_novelty_facts(&parent_state, &pfast, &pfull, &mut facts_buf);
        novelty.prepare_parent(
            (pnode.fast_eval.enemy_hp / 10, pnode.potion_count as i32),
            &facts_buf,
        );

        let mut tasks = Vec::new();
        let mut branch_truncations = 0u64;
        enumerate_parent_tasks(
            0,
            item.node_id,
            &pnode,
            p_has_la,
            p_la_score,
            &mut parent_state,
            content,
            policy,
            profile,
            &sensitivity,
            &root_baseline,
            true,
            &mut tasks,
            &mut por_pruned,
            &mut branch_truncations,
        );

        scratch.candidates.clear();
        {
            let mut probe_access = ProbeCacheAccess::SingleThread(&mut probe_cache);
            for task in &tasks {
                let _ = execute_action_from_parent(
                    &mut scratch,
                    &parent_state,
                    task,
                    0,
                    false,
                    false,
                    &root.static_ctx,
                    content,
                    &root_baseline,
                    policy,
                    profile,
                    cert,
                    &frozen_incumbent,
                    &tt,
                    &mut probe_access,
                    &sensitivity,
                    true,
                    EvalMode::Balanced,
                );
                if let Some(err) = scratch.fatal_error.take() {
                    abort.record_error(err);
                    return Err(abort.clone_error().expect("abort error recorded"));
                }
            }
        }

        let children: Vec<WorkerCandidate> = scratch.candidates.drain(..).collect();
        for cand in children {
            let (choices_offset, choices_len) = arena.choices.extend_from_arena_slice(
                &scratch.staged_choices,
                cand.choices_offset,
                cand.choices_len,
            );
            if cand.is_terminal {
                let mut node_flags = NODE_FLAG_IS_TERMINAL;
                if cand.has_extra_end_turn {
                    node_flags |= NODE_FLAG_HAS_EXTRA_END_TURN;
                }
                let rare_eval_idx = arena.push_opt_rare_eval(cand.rare_eval);
                let mut node = SearchNode {
                    parent_raw: cand.parent_id.0,
                    action: cand.action,
                    por_key: cand.por_key,
                    choices_offset,
                    choices_len,
                    state_handle: PackedHandle::default(),
                    state_hash: cand.state_hash,
                    beam_rank_score: cand.base_score,
                    turn: cand.turn.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                    action_count: cand.action_count,
                    potion_count: cand.potion_count.min(u8::MAX as u16) as u8,
                    explicit_potion_count: cand.explicit_potion_count.min(u8::MAX as u16) as u8,
                    optional_paid_potion_count: cand
                        .optional_paid_potion_count
                        .min(u8::MAX as u16) as u8,
                    automatic_potion_count: cand.automatic_potion_count.min(u8::MAX as u16)
                        as u8,
                    used_potion_slot_mask: cand.used_potion_slot_mask,
                    potion_strategic_cost: cand
                        .potion_strategic_cost
                        .clamp(i16::MIN as i32, i16::MAX as i32)
                        as i16,
                    optional_paid_potion_cost: cand
                        .optional_paid_potion_cost
                        .clamp(i16::MIN as i32, i16::MAX as i32)
                        as i16,
                    ambergris_count: cand.ambergris_count.min(u8::MAX as u16) as u8,
                    forced_used_mask: cand.forced_used_mask,
                    forced_ambergris_count: cand.forced_ambergris_count.min(u8::MAX as u16)
                        as u8,
                    node_flags,
                    fast_eval: cand.fast_eval,
                    rare_eval_idx,
                    ..Default::default()
                };
                if let Some(ref f) = cand.full_eval {
                    node.set_full_eval(f);
                }
                let cid = arena.push_node(node);
                completed.push(cid);
                if has_reached_acceptable_battle_hp_loss_node(
                    &root_baseline,
                    policy,
                    &arena,
                    cid,
                ) {
                    termination = "HpTarget";
                    break;
                }
                continue;
            }
            let state_bytes = scratch.slab.get(cand.state_slab_idx, cand.state_off, cand.state_len);
            // SAFETY: `state_bytes` was written by `pack_state` in this process.
            if unsafe { unpack_state(state_bytes, &root.static_ctx, &mut scratch.state) }.is_err() {
                continue;
            }
            let cfast = evaluate_fast(&scratch.state, content, &root_baseline, None);
            let cfull = evaluate_full(&scratch.state, content, &cfast, &root_baseline);
            capture_novelty_facts(&scratch.state, &cfast, &cfull, &mut facts_buf);
            let w = novelty.evaluate(
                (cand.fast_eval.enemy_hp / 10, cand.potion_count as i32),
                &facts_buf,
            );

            let rare_eval_idx = arena.push_opt_rare_eval(cand.rare_eval);
            let mut node_flags = 0u8;
            if cand.has_extra_end_turn {
                node_flags |= NODE_FLAG_HAS_EXTRA_END_TURN;
            }
            let mut node = SearchNode {
                parent_raw: cand.parent_id.0,
                action: cand.action,
                por_key: cand.por_key,
                choices_offset,
                choices_len,
                state_handle: main_arena.commit_slice(state_bytes),
                state_hash: cand.state_hash,
                beam_rank_score: cand.base_score,
                turn: cand.turn.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                action_count: cand.action_count,
                potion_count: cand.potion_count.min(u8::MAX as u16) as u8,
                explicit_potion_count: cand.explicit_potion_count.min(u8::MAX as u16) as u8,
                optional_paid_potion_count: cand
                    .optional_paid_potion_count
                    .min(u8::MAX as u16) as u8,
                automatic_potion_count: cand.automatic_potion_count.min(u8::MAX as u16) as u8,
                used_potion_slot_mask: cand.used_potion_slot_mask,
                potion_strategic_cost: cand
                    .potion_strategic_cost
                    .clamp(i16::MIN as i32, i16::MAX as i32)
                    as i16,
                optional_paid_potion_cost: cand
                    .optional_paid_potion_cost
                    .clamp(i16::MIN as i32, i16::MAX as i32)
                    as i16,
                ambergris_count: cand.ambergris_count.min(u8::MAX as u16) as u8,
                forced_used_mask: cand.forced_used_mask,
                forced_ambergris_count: cand.forced_ambergris_count.min(u8::MAX as u16) as u8,
                node_flags,
                fast_eval: cand.fast_eval,
                rare_eval_idx,
                ..Default::default()
            };
            node.set_full_eval(&cfull.to_stored());
            let cid = arena.push_node(node);

            let mut admitted = w <= options.width;
            if !admitted {
                if let Some(ei) = item.escape_idx {
                    admitted = escapes[ei].try_admit();
                }
            }
            if !admitted {
                continue;
            }
            let child_escape = if options.familiar_allowance > 0 {
                if w <= options.width {
                    let i = escapes.len();
                    escapes.push(BfwsEscapeBudget::new(options.familiar_allowance));
                    Some(i)
                } else {
                    item.escape_idx
                }
            } else {
                None
            };
            open.enqueue(
                NoveltyOpenNode {
                    node_id: cid,
                    escape_idx: child_escape,
                },
                w as i32,
                cand.base_score,
                sequence,
            );
            sequence += 1;
        }
        if termination == "HpTarget" || novelty.limit_reached {
            break;
        }
    }

    let selected = select_final_plan(
        &arena,
        &completed,
        &root_baseline,
        policy.potion_policy,
        Some(policy.theft_policy),
        root_baseline.potion_replacement_hp_credit,
        root_baseline.minimum_required_potion_uses,
    );
    let (
        best_node,
        best_turn,
        won,
        survives,
        is_event_defeat,
        death_save_use_count,
        player_hp,
        end_player_max_hp,
        all_forced_uses_satisfied,
        strategic_hp_deficit,
        net_persistent_hp_delta_centi,
        net_potion_opportunity_cost_centi,
        strategy_goal_hp_credit,
        strategy_goal_count,
        growth_rewards,
        relic_eval,
        recovered_hp,
        hp_lost,
        potion_count,
        optional_potion_count,
        optional_paid_potion_count,
        optional_potion_strategic_cost,
        optional_ambergris_count,
        used_potion_slot_mask,
        alchemize_count,
        empty_potion_slots,
        outstanding_stolen,
        forfeits_gold,
        score,
        actions,
    ) = match selected {
        Some(sel) => (
            Some(sel.node_id),
            sel.turn,
            sel.won,
            sel.survives,
            sel.is_event_defeat,
            sel.death_save_use_count,
            sel.player_hp,
            sel.end_player_max_hp,
            sel.all_forced_uses_satisfied,
            sel.strategic_hp_deficit,
            sel.net_persistent_hp_delta_centi,
            sel.net_potion_opportunity_cost_centi,
            sel.strategy_goal_hp_credit,
            sel.strategy_goal_count,
            sel.growth_rewards,
            sel.relic_eval,
            sel.recovered_hp,
            sel.hp_lost,
            sel.potion_count,
            sel.optional_potion_count,
            sel.optional_paid_potion_count,
            sel.optional_potion_strategic_cost,
            sel.optional_ambergris_count,
            sel.used_potion_slot_mask,
            sel.alchemize_count,
            sel.empty_potion_slots,
            sel.outstanding_stolen,
            sel.forfeits_gold,
            sel.score,
            arena.reconstruct_actions(sel.node_id),
        ),
        None => (
            None,
            root.player.turn_number,
            false,
            false,
            false,
            0,
            0,
            root_baseline.initial_player_max_hp,
            root_baseline.all_forced_potion_mask == 0,
            i32::MAX,
            i32::MIN / 2,
            0,
            0,
            0,
            GrowthCounters::default(),
            RelicCounterEvaluation::default(),
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            root_baseline.empty_root_potion_slots,
            0,
            false,
            0,
            Vec::new(),
        ),
    };

    let total_committed = main_arena.total_bytes_committed;
    let default_enemy_hp: i32 = root
        .enemy_handles
        .iter()
        .filter_map(|&eh| root.creature(eh))
        .filter(|c| c.is_alive())
        .map(|c| c.current_hp.max(0))
        .sum();
    let (explicit_potion_count, raw_optional_paid_potion_cost, enemy_hp, all_strategy_targets_satisfied) =
        match best_node {
            Some(nid) => {
                let n = arena.get(nid);
                let rare = arena.rare_eval(nid);
                (
                    n.explicit_potion_count as usize,
                    n.optional_paid_potion_cost as i32,
                    n.fast_eval.enemy_hp,
                    are_strategy_targets_satisfied_raw(&root_baseline, &n.fast_eval, &rare),
                )
            }
            None => (
                0,
                0,
                default_enemy_hp,
                !root_baseline.effective_has_growth_targets
                    && root_baseline.active_relic_targets.is_empty(),
            ),
        };
    let nodes_in_arena = arena.len();
    Ok(BeamRunOutput {
        best_node,
        best_turn,
        nodes_in_arena,
        root_baseline,
        won,
        survives,
        is_event_defeat,
        death_save_use_count,
        player_hp,
        end_player_max_hp,
        enemy_hp,
        all_forced_uses_satisfied,
        all_strategy_targets_satisfied,
        strategic_hp_deficit,
        net_persistent_hp_delta_centi,
        net_potion_opportunity_cost_centi,
        strategy_goal_hp_credit,
        strategy_goal_count,
        growth_rewards,
        relic_eval,
        recovered_hp,
        hp_lost,
        potion_count,
        explicit_potion_count,
        optional_potion_count,
        optional_paid_potion_count,
        optional_potion_strategic_cost,
        raw_optional_paid_potion_cost,
        optional_ambergris_count,
        used_potion_slot_mask,
        alchemize_count,
        empty_potion_slots,
        outstanding_stolen,
        forfeits_gold,
        has_completed_turn_ongoing: false,
        completed_ongoing_turn: -1,
        score,
        actions,
        expanded_nodes,
        transition_count: scratch.transitions,
        choice_replay_attempts: scratch.choice_replay_attempts,
        tt_pruned: tt.branches_pruned,
        incumbent_pruned: scratch.incumbent_pruned,
        por_pruned,
        packed_state_bytes_total: total_committed,
        termination,
        was_losslessly_exhausted: false,
        terminal_cards_hint: Vec::new(),
        early_turn_frontiers: Vec::new(),
        boss_tempo_prefixes: Vec::new(),
    })
}

#[cold]
#[inline(never)]
fn run_boss_tempo_depth_first(
    root: &BattleState,
    content: &(dyn ContentDispatcher + Sync),
    policy: &SearchPolicy,
    profile: &SolverSearchProfile,
    cert: &ClosureCertificate,
    _incumbent_table: &PrimaryIncumbentTable,
    cancel: &AtomicBool,
    abort: &SearchAbortSlot,
    options: &BossTempoSearchOptions,
) -> Result<BeamRunOutput, KernelError> {
    let started = Instant::now();
    let root_baseline = RootEvalBaseline::from_state_with_policy(root, content, policy);
    let sensitivity = CombatOrderSensitivity::analyze(root, content);
    let mut arena = NodeArena::default();
    let mut main_arena = MainStateArena::default();
    let mut scratch = WorkerScratch::new(root.static_ctx.clone());
    let mut parent_state = empty_scratch_state(root.static_ctx.clone());
    let tt = ShardedParetoTT::new(16_384);
    let mut probe_cache = ChoiceProbeCache::new();
    let frozen_incumbent = FrozenIncumbentBound::default();
    let mut tempo_tt = BossTempoTranspositions::new(32_768);
    let mut boss_profile = profile.clone();
    boss_profile.boss_tempo_hp_pricing = true;

    pack_state(root, &mut scratch.pack_buf);
    let root_hash = hash_canonical_key(root);
    let root_fast = evaluate_fast(root, content, &root_baseline, None);
    let root_full = evaluate_full(root, content, &root_fast, &root_baseline);
    let root_rare_idx = arena.push_rare_eval(RareNodeEval {
        growth_rewards: root_fast.growth_rewards,
        relic_eval: root_fast.relic_eval,
    });
    let root_terminal =
        !root.is_in_progress || root_fast.won || root_fast.player_dead || root_fast.is_event_defeat;
    let mut root_node = SearchNode {
        state_handle: main_arena.commit_slice(&scratch.pack_buf),
        state_hash: root_hash,
        beam_rank_score: root_fast.base_score,
        turn: root
            .player
            .turn_number
            .clamp(i16::MIN as i32, i16::MAX as i32) as i16,
        node_flags: if root_terminal {
            NODE_FLAG_IS_TERMINAL
        } else {
            0
        },
        fast_eval: StoredFastEval::from(&root_fast),
        rare_eval_idx: root_rare_idx,
        ..Default::default()
    };
    root_node.set_full_eval(&root_full.to_stored());
    let root_id = arena.push_node(root_node);

    let mut pending = vec![BossTempoPendingItem {
        node_id: root_id,
        remaining_discrepancies: options.discrepancy_allowance,
    }];
    let mut completed = Vec::new();
    let mut expanded_nodes = 0usize;
    let mut por_pruned = 0u64;
    let mut termination = "IterationExhausted";

    while let Some(item) = pending.pop() {
        if let Some(err) = abort.clone_error() {
            return Err(err);
        }
        if cancel.load(Ordering::Relaxed) {
            termination = "Cancelled";
            break;
        }
        if expanded_nodes >= profile.max_expanded_nodes {
            termination = BeamWidthPortfolio::NODE_LIMIT_TERMINATION;
            break;
        }
        if profile.soft_time_budget_ms > 0
            && (started.elapsed().as_millis() as u64) >= profile.soft_time_budget_ms
        {
            termination = "TimeLimit";
            break;
        }
        let pnode = arena.get(item.node_id).clone();
        if pnode.is_terminal() {
            completed.push(item.node_id);
            continue;
        }
        if !tempo_tt.try_mark(
            pnode.state_hash,
            item.remaining_discrepancies,
            pnode.fast_eval.transposition_label(
                pnode.beam_rank_score_or_base(),
                pnode.potion_count,
                pnode.optional_paid_potion_count,
                pnode.explicit_potion_count > 0,
                pnode.potion_strategic_cost as i32,
                0,
                pnode.action_count,
                pnode.forced_used_mask,
            ),
        ) {
            continue;
        }
        expanded_nodes += 1;
        let pbytes = main_arena.get(pnode.state_handle);
        // SAFETY: `pbytes` was committed from `pack_state` output in this process.
        if unsafe { unpack_state(pbytes, &root.static_ctx, &mut parent_state) }.is_err() {
            continue;
        }
        let p_has_la = pnode.has_choice_lookahead();
        let p_la_score = pnode.choice_lookahead_score();
        let mut tasks = Vec::new();
        let mut branch_truncations = 0u64;
        enumerate_parent_tasks(
            0,
            item.node_id,
            &pnode,
            p_has_la,
            p_la_score,
            &mut parent_state,
            content,
            policy,
            &boss_profile,
            &sensitivity,
            &root_baseline,
            true,
            &mut tasks,
            &mut por_pruned,
            &mut branch_truncations,
        );
        scratch.candidates.clear();
        {
            let mut probe_access = ProbeCacheAccess::SingleThread(&mut probe_cache);
            for task in &tasks {
                let _ = execute_action_from_parent(
                    &mut scratch,
                    &parent_state,
                    task,
                    0,
                    false,
                    false,
                    &root.static_ctx,
                    content,
                    &root_baseline,
                    policy,
                    &boss_profile,
                    cert,
                    &frozen_incumbent,
                    &tt,
                    &mut probe_access,
                    &sensitivity,
                    true,
                    EvalMode::Balanced,
                );
                if let Some(err) = scratch.fatal_error.take() {
                    abort.record_error(err);
                    return Err(abort.clone_error().expect("abort error recorded"));
                }
            }
        }

        let mut next_nodes = Vec::new();
        for cand in scratch.candidates.drain(..) {
            let state_handle = if cand.is_terminal {
                PackedHandle::default()
            } else {
                let state_bytes =
                    scratch.slab.get(cand.state_slab_idx, cand.state_off, cand.state_len);
                main_arena.commit_slice(state_bytes)
            };
            let rank_score = cand.full_eval.map_or(cand.base_score, |f| f.beam_rank_score);
            let rare_eval_idx = arena.push_opt_rare_eval(cand.rare_eval);
            let (choices_offset, choices_len) = arena.choices.extend_from_arena_slice(
                &scratch.staged_choices,
                cand.choices_offset,
                cand.choices_len,
            );
            let mut node_flags = 0u8;
            if cand.has_extra_end_turn {
                node_flags |= NODE_FLAG_HAS_EXTRA_END_TURN;
            }
            if cand.is_terminal {
                node_flags |= NODE_FLAG_IS_TERMINAL;
            }
            let mut node = SearchNode {
                parent_raw: cand.parent_id.0,
                action: cand.action,
                por_key: cand.por_key,
                choices_offset,
                choices_len,
                state_handle,
                state_hash: cand.state_hash,
                beam_rank_score: cand.base_score,
                turn: cand.turn.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                action_count: cand.action_count,
                potion_count: cand.potion_count.min(u8::MAX as u16) as u8,
                explicit_potion_count: cand.explicit_potion_count.min(u8::MAX as u16) as u8,
                optional_paid_potion_count: cand
                    .optional_paid_potion_count
                    .min(u8::MAX as u16) as u8,
                automatic_potion_count: cand.automatic_potion_count.min(u8::MAX as u16) as u8,
                used_potion_slot_mask: cand.used_potion_slot_mask,
                potion_strategic_cost: cand
                    .potion_strategic_cost
                    .clamp(i16::MIN as i32, i16::MAX as i32)
                    as i16,
                optional_paid_potion_cost: cand
                    .optional_paid_potion_cost
                    .clamp(i16::MIN as i32, i16::MAX as i32)
                    as i16,
                ambergris_count: cand.ambergris_count.min(u8::MAX as u16) as u8,
                forced_used_mask: cand.forced_used_mask,
                forced_ambergris_count: cand.forced_ambergris_count.min(u8::MAX as u16) as u8,
                node_flags,
                fast_eval: cand.fast_eval,
                rare_eval_idx,
                ..Default::default()
            };
            if let Some(ref cfull) = cand.full_eval {
                node.set_full_eval(cfull);
            }
            let cid = arena.push_node(node);
            if cand.is_terminal {
                completed.push(cid);
            } else if !cand.fast_eval.player_dead() {
                next_nodes.push((cid, cand.turn > (pnode.turn as i32), rank_score));
            }
        }
        next_nodes.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| b.2.total_cmp(&a.2)));
        if let Some(&(best_cid, _, _)) = next_nodes.first() {
            for &(cid, _, _) in next_nodes.iter().skip(1).rev() {
                if item.remaining_discrepancies > 0 {
                    pending.push(BossTempoPendingItem {
                        node_id: cid,
                        remaining_discrepancies: item.remaining_discrepancies - 1,
                    });
                }
            }
            pending.push(BossTempoPendingItem {
                node_id: best_cid,
                remaining_discrepancies: item.remaining_discrepancies,
            });
        }
    }

    let selected = select_final_plan(
        &arena,
        &completed,
        &root_baseline,
        policy.potion_policy,
        Some(policy.theft_policy),
        root_baseline.potion_replacement_hp_credit,
        root_baseline.minimum_required_potion_uses,
    );
    let (
        best_node,
        best_turn,
        won,
        survives,
        is_event_defeat,
        death_save_use_count,
        player_hp,
        end_player_max_hp,
        all_forced_uses_satisfied,
        strategic_hp_deficit,
        net_persistent_hp_delta_centi,
        net_potion_opportunity_cost_centi,
        strategy_goal_hp_credit,
        strategy_goal_count,
        growth_rewards,
        relic_eval,
        recovered_hp,
        hp_lost,
        potion_count,
        optional_potion_count,
        optional_paid_potion_count,
        optional_potion_strategic_cost,
        optional_ambergris_count,
        used_potion_slot_mask,
        alchemize_count,
        empty_potion_slots,
        outstanding_stolen,
        forfeits_gold,
        score,
        actions,
    ) = match selected {
        Some(sel) => (
            Some(sel.node_id),
            sel.turn,
            sel.won,
            sel.survives,
            sel.is_event_defeat,
            sel.death_save_use_count,
            sel.player_hp,
            sel.end_player_max_hp,
            sel.all_forced_uses_satisfied,
            sel.strategic_hp_deficit,
            sel.net_persistent_hp_delta_centi,
            sel.net_potion_opportunity_cost_centi,
            sel.strategy_goal_hp_credit,
            sel.strategy_goal_count,
            sel.growth_rewards,
            sel.relic_eval,
            sel.recovered_hp,
            sel.hp_lost,
            sel.potion_count,
            sel.optional_potion_count,
            sel.optional_paid_potion_count,
            sel.optional_potion_strategic_cost,
            sel.optional_ambergris_count,
            sel.used_potion_slot_mask,
            sel.alchemize_count,
            sel.empty_potion_slots,
            sel.outstanding_stolen,
            sel.forfeits_gold,
            sel.score,
            arena.reconstruct_actions(sel.node_id),
        ),
        None => (
            None,
            root.player.turn_number,
            false,
            false,
            false,
            0,
            0,
            root_baseline.initial_player_max_hp,
            root_baseline.all_forced_potion_mask == 0,
            i32::MAX,
            i32::MIN / 2,
            0,
            0,
            0,
            GrowthCounters::default(),
            RelicCounterEvaluation::default(),
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            root_baseline.empty_root_potion_slots,
            0,
            false,
            0,
            Vec::new(),
        ),
    };

    let total_committed = main_arena.total_bytes_committed;
    let default_enemy_hp: i32 = root
        .enemy_handles
        .iter()
        .filter_map(|&eh| root.creature(eh))
        .filter(|c| c.is_alive())
        .map(|c| c.current_hp.max(0))
        .sum();
    let (explicit_potion_count, raw_optional_paid_potion_cost, enemy_hp, all_strategy_targets_satisfied) =
        match best_node {
            Some(nid) => {
                let n = arena.get(nid);
                let rare = arena.rare_eval(nid);
                (
                    n.explicit_potion_count as usize,
                    n.optional_paid_potion_cost as i32,
                    n.fast_eval.enemy_hp,
                    are_strategy_targets_satisfied_raw(&root_baseline, &n.fast_eval, &rare),
                )
            }
            None => (
                0,
                0,
                default_enemy_hp,
                !root_baseline.effective_has_growth_targets
                    && root_baseline.active_relic_targets.is_empty(),
            ),
        };
    let nodes_in_arena = arena.len();
    Ok(BeamRunOutput {
        best_node,
        best_turn,
        nodes_in_arena,
        root_baseline,
        won,
        survives,
        is_event_defeat,
        death_save_use_count,
        player_hp,
        end_player_max_hp,
        enemy_hp,
        all_forced_uses_satisfied,
        all_strategy_targets_satisfied,
        strategic_hp_deficit,
        net_persistent_hp_delta_centi,
        net_potion_opportunity_cost_centi,
        strategy_goal_hp_credit,
        strategy_goal_count,
        growth_rewards,
        relic_eval,
        recovered_hp,
        hp_lost,
        potion_count,
        explicit_potion_count,
        optional_potion_count,
        optional_paid_potion_count,
        optional_potion_strategic_cost,
        raw_optional_paid_potion_cost,
        optional_ambergris_count,
        used_potion_slot_mask,
        alchemize_count,
        empty_potion_slots,
        outstanding_stolen,
        forfeits_gold,
        has_completed_turn_ongoing: false,
        completed_ongoing_turn: -1,
        score,
        actions,
        expanded_nodes,
        transition_count: scratch.transitions,
        choice_replay_attempts: scratch.choice_replay_attempts,
        tt_pruned: tt.branches_pruned,
        incumbent_pruned: scratch.incumbent_pruned,
        por_pruned,
        packed_state_bytes_total: total_committed,
        termination,
        was_losslessly_exhausted: false,
        terminal_cards_hint: Vec::new(),
        early_turn_frontiers: Vec::new(),
        boss_tempo_prefixes: Vec::new(),
    })
}

#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn has_reached_acceptable_battle_hp_loss_raw(
    root: &RootEvalBaseline,
    policy: &SearchPolicy,
    f: &StoredFastEval,
    rare: &RareNodeEval,
    action_count: u16,
    forced_used_mask: u64,
    optional_paid_potion_count: u8,
    explicit_potion_count: u8,
) -> bool {
    let complete_victory = !f.is_event_defeat()
        && crate::eval::RouteQualityPolicy::is_complete_victory(
            action_count as usize,
            f.all_enemies_dead() || f.won(),
            f.player_dead(),
            f.projected_player_hp as i32,
        );
    let check_theft = policy.theft_policy == SolverTheftPolicy::PreserveResources;
    let all_forced_ok =
        (forced_used_mask & root.all_forced_potion_mask) == root.all_forced_potion_mask;
    let policy_ok = match root.effective_potion_policy {
        SolverPotionPolicy::Disabled => {
            optional_paid_potion_count == 0
                && (explicit_potion_count as usize) <= root.forced_directive_count
        }
        SolverPotionPolicy::RequireAtLeastOne => {
            (explicit_potion_count as usize) >= root.minimum_required_potion_uses
        }
        SolverPotionPolicy::Smart => true,
    };
    root.can_stop_at_hp_target
        && complete_victory
        && !f.player_dead()
        && f.projected_player_hp > 0
        && f.death_save_use_count == 0
        && (!check_theft || (f.outstanding_stolen == 0 && !f.forfeits_gold()))
        && all_forced_ok
        && policy_ok
        && are_strategy_targets_satisfied_raw(root, f, rare)
        && optional_paid_potion_count == 0
        && projected_battle_hp_lost_from_cumulative(root, f.cumulative_hp_lost as i32)
            <= policy.acceptable_battle_hp_loss
}

fn has_reached_acceptable_battle_hp_loss_node(
    root: &RootEvalBaseline,
    policy: &SearchPolicy,
    arena: &NodeArena,
    nid: NodeId,
) -> bool {
    let node = arena.get(nid);
    let rare = arena.rare_eval(nid);
    has_reached_acceptable_battle_hp_loss_raw(
        root,
        policy,
        &node.fast_eval,
        &rare,
        node.action_count,
        node.forced_used_mask,
        node.optional_paid_potion_count,
        node.explicit_potion_count,
    )
}

fn has_reached_acceptable_battle_hp_loss(
    root: &RootEvalBaseline,
    policy: &SearchPolicy,
    out: &BeamRunOutput,
) -> bool {
    let check_theft = policy.theft_policy == SolverTheftPolicy::PreserveResources;
    let policy_ok = match root.effective_potion_policy {
        SolverPotionPolicy::Disabled => {
            out.optional_paid_potion_count == 0
                && out.explicit_potion_count <= root.forced_directive_count
        }
        SolverPotionPolicy::RequireAtLeastOne => {
            out.explicit_potion_count >= root.minimum_required_potion_uses
        }
        SolverPotionPolicy::Smart => true,
    };
    root.can_stop_at_hp_target
        && out.won
        && out.survives
        && out.death_save_use_count == 0
        && (!check_theft || (out.outstanding_stolen == 0 && !out.forfeits_gold))
        && out.all_forced_uses_satisfied
        && policy_ok
        && are_strategy_targets_satisfied_output(root, out)
        && out.optional_paid_potion_count == 0
        && projected_battle_hp_lost_from_cumulative(root, out.hp_lost)
            <= policy.acceptable_battle_hp_loss
}

fn can_finish_target_portfolio(
    root: &RootEvalBaseline,
    policy: &SearchPolicy,
    profile: &SolverSearchProfile,
    out: &BeamRunOutput,
    cert: &ClosureCertificate,
) -> bool {
    profile.stop_portfolio_at_hp_target
        && cert.healing_certified
        && out.recovered_hp == 0
        && has_reached_acceptable_battle_hp_loss(root, policy, out)
}

#[inline]
fn satisfies_run_output_hard_constraints(out: &BeamRunOutput) -> bool {
    let root = &out.root_baseline;
    let require_hard = root.has_active_hard_potion_constraint();
    if require_hard && out.best_node.is_none() {
        return false;
    }
    let policy_ok = match root.effective_potion_policy {
        SolverPotionPolicy::Disabled => {
            out.optional_paid_potion_count == 0
                && out.explicit_potion_count <= root.forced_directive_count
        }
        SolverPotionPolicy::RequireAtLeastOne => {
            !require_hard || out.explicit_potion_count >= root.minimum_required_potion_uses
        }
        SolverPotionPolicy::Smart => true,
    };
    out.all_forced_uses_satisfied
        && (!require_hard || out.explicit_potion_count >= root.minimum_required_potion_uses)
        && policy_ok
}

fn is_better_run_output(cand: &BeamRunOutput, cur: &BeamRunOutput) -> bool {
    is_better_run_output_with_policy(cand, cur, None, None)
}

fn is_better_run_output_with_policy(
    cand: &BeamRunOutput,
    cur: &BeamRunOutput,
    policy: Option<&SearchPolicy>,
    _zero_potion_baseline: Option<&BeamRunOutput>,
) -> bool {
    let cand_hard = satisfies_run_output_hard_constraints(cand);
    let cur_hard = satisfies_run_output_hard_constraints(cur);
    if cand_hard != cur_hard {
        return cand_hard;
    }
    if cand.all_forced_uses_satisfied != cur.all_forced_uses_satisfied {
        return cand.all_forced_uses_satisfied;
    }
    if !cand.won && !cur.won {
        // Do not let a budget-truncated partial ongoing pass overwrite a complete EventDefeat pass.
        if cur.termination == "EventDefeat"
            && cand.termination != "None"
            && cand.termination != "EventDefeat"
        {
            return false;
        }
        if cand.termination == "EventDefeat"
            && cur.termination != "None"
            && cur.termination != "EventDefeat"
        {
            return true;
        }
        if cand.has_completed_turn_ongoing != cur.has_completed_turn_ongoing {
            return cand.has_completed_turn_ongoing;
        }
        if cand.has_completed_turn_ongoing
            && cand.survives
            && cur.survives
            && cand.completed_ongoing_turn != cur.completed_ongoing_turn
        {
            if cand.completed_ongoing_turn > cur.completed_ongoing_turn && cur.termination == "None"
            {
                return true;
            }
            if cur.completed_ongoing_turn > cand.completed_ongoing_turn && cand.termination == "None"
            {
                return false;
            }
        }
    }
    if cand.won != cur.won {
        return cand.won;
    }
    if cand.survives != cur.survives {
        return cand.survives;
    }
    if cand.is_event_defeat != cur.is_event_defeat {
        return !cand.is_event_defeat;
    }
    if cand.death_save_use_count != cur.death_save_use_count {
        return cand.death_save_use_count < cur.death_save_use_count;
    }

    let check_theft = policy
        .map_or(cand.root_baseline.theft_policy, |p| p.theft_policy)
        == SolverTheftPolicy::PreserveResources;
    if check_theft {
        if cand.outstanding_stolen != cur.outstanding_stolen {
            return cand.outstanding_stolen < cur.outstanding_stolen;
        }
        if cand.forfeits_gold != cur.forfeits_gold {
            return !cand.forfeits_gold;
        }
    }

    if !cand.won && !cand.survives && !cur.won && !cur.survives {
        if cand.net_potion_opportunity_cost_centi != cur.net_potion_opportunity_cost_centi {
            return cand.net_potion_opportunity_cost_centi < cur.net_potion_opportunity_cost_centi;
        }
        if cand.optional_paid_potion_count != cur.optional_paid_potion_count {
            return cand.optional_paid_potion_count < cur.optional_paid_potion_count;
        }
        if cand.enemy_hp != cur.enemy_hp {
            return cand.enemy_hp < cur.enemy_hp;
        }
        if cand.best_turn != cur.best_turn {
            return cand.best_turn > cur.best_turn;
        }
        return cand.score > cur.score;
    }

    let cand_v = (cand.net_persistent_hp_delta_centi as i64)
        - (cand.net_potion_opportunity_cost_centi as i64);
    let cur_v = (cur.net_persistent_hp_delta_centi as i64)
        - (cur.net_potion_opportunity_cost_centi as i64);
    if cand_v != cur_v {
        return cand_v > cur_v;
    }
    if cand.net_persistent_hp_delta_centi != cur.net_persistent_hp_delta_centi {
        return cand.net_persistent_hp_delta_centi > cur.net_persistent_hp_delta_centi;
    }
    if cand.strategy_goal_hp_credit != cur.strategy_goal_hp_credit {
        return cand.strategy_goal_hp_credit > cur.strategy_goal_hp_credit;
    }
    if cand.strategy_goal_count != cur.strategy_goal_count {
        return cand.strategy_goal_count > cur.strategy_goal_count;
    }
    let cand_battle_hp_lost =
        projected_battle_hp_lost_from_cumulative(&cand.root_baseline, cand.hp_lost);
    let cur_battle_hp_lost =
        projected_battle_hp_lost_from_cumulative(&cur.root_baseline, cur.hp_lost);
    if cand_battle_hp_lost != cur_battle_hp_lost {
        return cand_battle_hp_lost < cur_battle_hp_lost;
    }
    if cand.potion_count != cur.potion_count {
        return cand.potion_count < cur.potion_count;
    }
    if cand.best_turn != cur.best_turn {
        return cand.best_turn < cur.best_turn;
    }
    if cand.enemy_hp != cur.enemy_hp {
        return cand.enemy_hp < cur.enemy_hp;
    }
    cand.score > cur.score
}

#[inline]
fn build_paid_potion_improve_input(
    base: &BeamRunOutput,
    root_baseline: &RootEvalBaseline,
    check_theft: bool,
) -> PaidPotionImproveInput {
    PaidPotionImproveInput {
        won: base.won,
        survives: base.survives,
        death_save_use_count: base.death_save_use_count,
        end_player_hp: base.player_hp,
        end_player_max_hp: base.end_player_max_hp,
        used_potion_slot_mask: base.used_potion_slot_mask,
        optional_paid_potion_count: base.optional_paid_potion_count.min(u8::MAX as usize) as u8,
        raw_potion_strategic_cost: base.raw_optional_paid_potion_cost,
        net_potion_opportunity_cost_centi: base.net_potion_opportunity_cost_centi,
        alchemize_count: base.alchemize_count,
        empty_potion_slots: base.empty_potion_slots,
        all_strategy_targets_satisfied: are_strategy_targets_satisfied_output(root_baseline, base),
        all_growth_targets_satisfied: !root_baseline.effective_has_growth_targets
            || root_baseline
                .growth_opportunity_targets
                .is_satisfied_by(&base.growth_rewards),
        growth_hp_credit: base.strategy_goal_hp_credit,
        relic_satisfied_mask: base.relic_eval.satisfied_mask as u32,
        outstanding_stolen: base.outstanding_stolen,
        forfeits_gold: base.forfeits_gold,
        theft_preserve_resources: check_theft,
    }
}

#[inline]
fn can_any_paid_potion_improve_output(
    root_baseline: &RootEvalBaseline,
    out: &BeamRunOutput,
    check_theft: bool,
) -> bool {
    PotionUsePolicy::can_any_paid_potion_improve_on(
        build_paid_potion_improve_input(out, root_baseline, check_theft),
        root_baseline,
    )
}

fn should_run_smart_potion_pass(
    zero_baseline: Option<&BeamRunOutput>,
    root_baseline: &RootEvalBaseline,
    check_theft: bool,
) -> bool {
    if root_baseline.effective_potion_policy != SolverPotionPolicy::Smart
        || root_baseline.paid_optional_potion_count == 0
    {
        return false;
    }
    let Some(base) = zero_baseline else {
        return true;
    };
    if !base.won || !base.survives || base.death_save_use_count > 0 {
        return true;
    }
    if check_theft && (base.outstanding_stolen > 0 || base.forfeits_gold) {
        return true;
    }
    if !base.all_forced_uses_satisfied {
        return true;
    }
    can_any_paid_potion_improve_output(root_baseline, base, check_theft)
}

#[inline]
fn is_unimprovable_paid_win(
    stage_number: usize,
    selected_best: Option<&BeamRunOutput>,
    root_baseline: &RootEvalBaseline,
    check_theft: bool,
) -> bool {
    let Some(best) = selected_best else {
        return false;
    };
    let has_any_strategy_targets = root_baseline.effective_has_growth_targets
        || !root_baseline.active_relic_targets.is_empty();
    let battle_hp_lost = projected_battle_hp_lost_from_cumulative(root_baseline, best.hp_lost);
    stage_number > 1
        && best.won
        && best.survives
        && best.death_save_use_count == 0
        && best.optional_paid_potion_count > 0
        && battle_hp_lost <= 0
        && best.strategic_hp_deficit <= 0
        && (!check_theft || (best.outstanding_stolen == 0 && !best.forfeits_gold))
        && satisfies_run_output_hard_constraints(best)
        && (!has_any_strategy_targets || are_strategy_targets_satisfied_output(root_baseline, best))
        && best.raw_optional_paid_potion_cost <= root_baseline.min_paid_potion_cost
        && !can_any_paid_potion_improve_output(root_baseline, best, check_theft)
}

#[inline]
fn resolve_initial_beam_width(configured_beam_width: usize, root: &RootEvalBaseline) -> usize {
    let w = configured_beam_width.clamp(4, MAX_BEAM_WIDTH);
    if root.is_act_ending_boss && w < 45 {
        45
    } else {
        w
    }
}

#[allow(clippy::too_many_arguments)]
fn run_adaptive_stage(
    root: &BattleState,
    content: &(dyn ContentDispatcher + Sync),
    policy: &SearchPolicy,
    base_profile: &SolverSearchProfile,
    root_baseline_info: &RootEvalBaseline,
    cert: &ClosureCertificate,
    incumbent_table: &PrimaryIncumbentTable,
    shared_incumbent: &mut FrozenIncumbentBound,
    pool: Option<&rayon::ThreadPool>,
    cancel: &AtomicBool,
    abort: &SearchAbortSlot,
    progress_cb: Option<&(dyn Fn(&SearchProgress) + Sync)>,
    started: &Instant,
    stage_number: usize,
    stage_width: usize,
    stage_node_ceiling: usize,
    eval_mode: EvalMode,
    rollout_horizon_turns: usize,
    stage_deadline_ms: u64,
    full_deadline_ms: u64,
    can_scale_adaptive_stages: bool,
    allow_death_save_rescue: bool,
    stage1_phase_b_timed_out: &mut bool,
    stage1_phase_a_completed_cleanly: &mut bool,
    stage1_phase_a_losslessly_exhausted: &mut bool,
    post_best_boundary: &mut &'static str,
    total_expanded: &mut usize,
    total_transitions: &mut u64,
    total_choice_replays: &mut u64,
    total_tt_pruned: &mut u64,
    total_incumbent_pruned: &mut u64,
    total_por_pruned: &mut u64,
    baseline_zero_potion: &mut Option<BeamRunOutput>,
    selected_best: &mut Option<BeamRunOutput>,
    resources: &mut PassResources,
) -> Result<bool, KernelError> {
    if let Some(err) = abort.clone_error() {
        return Err(err);
    }
    let rem_ms_for = |deadline_ms: u64| -> Option<u64> {
        if deadline_ms == 0 {
            Some(0)
        } else {
            let elapsed = started.elapsed().as_millis() as u64;
            if elapsed >= deadline_ms {
                None
            } else {
                Some((deadline_ms - elapsed).max(1))
            }
        }
    };
    if stage_number > 1 {
        if rem_ms_for(stage_deadline_ms).is_none() {
            *post_best_boundary = "TimeLimit";
            return Ok(false);
        }
        if *total_expanded >= stage_node_ceiling {
            if *post_best_boundary != "TimeLimit" {
                *post_best_boundary = BeamWidthPortfolio::NODE_LIMIT_TERMINATION;
            }
            return Ok(false);
        }
    }
    let stage_available = stage_node_ceiling.saturating_sub(*total_expanded);
    if stage_available == 0 || cancel.load(Ordering::Relaxed) {
        return Ok(false);
    }
    let Some(avail_ms_before_a) = rem_ms_for(stage_deadline_ms) else {
        return Ok(false);
    };

    let smart_mode = root_baseline_info.effective_potion_policy == SolverPotionPolicy::Smart;
    let is_disabled_mode =
        root_baseline_info.effective_potion_policy == SolverPotionPolicy::Disabled;
    let check_theft = policy.theft_policy == SolverTheftPolicy::PreserveResources;
    let is_elite_or_boss = matches!(root.static_ctx.room_type, RoomType::Elite | RoomType::Boss);
    let reserve_for_paid_potions =
        smart_mode && root_baseline_info.paid_optional_potion_count > 0;
    let phase_a_allow_paid = !smart_mode && !is_disabled_mode;

    let has_clean_survival_best_before_a = selected_best
        .as_ref()
        .is_some_and(|b| b.won && b.survives && b.death_save_use_count == 0);
    let stage2_prioritize_phase_b = stage_number == 2
        && reserve_for_paid_potions
        && can_scale_adaptive_stages
        && has_clean_survival_best_before_a
        && *stage1_phase_a_completed_cleanly
        && (*stage1_phase_b_timed_out
            || selected_best.as_ref().is_some_and(|b| {
                projected_battle_hp_lost_from_cumulative(root_baseline_info, b.hp_lost) >= 20
            }))
        && baseline_zero_potion.is_some()
        && !is_unimprovable_paid_win(
            stage_number,
            selected_best.as_ref(),
            root_baseline_info,
            check_theft,
        )
        && should_run_smart_potion_pass(
            baseline_zero_potion.as_ref(),
            root_baseline_info,
            check_theft,
        );
    let skip_stage2_phase_a = stage2_prioritize_phase_b && *stage1_phase_a_losslessly_exhausted;

    let phase_a_was_losslessly_exhausted: bool;
    let phase_a_boundary_reason: &'static str;

    if !skip_stage2_phase_a {
        let (phase_a_percent, phase_a_time_percent) = if stage2_prioritize_phase_b {
            let p = if *stage1_phase_b_timed_out { 15 } else { 20 };
            (p, p)
        } else {
            let p = if is_elite_or_boss
                || (root_baseline_info.initial_player_hp as i64) * 100
                    <= (root_baseline_info.initial_player_max_hp as i64) * 60
            {
                50
            } else {
                60
            };
            (p, p + 5)
        };
        let min_turn_nodes = crate::eval::weights::MINIMUM_TURN_LAYER_EXPANDED_NODES;
        let phase_a_nodes = if reserve_for_paid_potions {
            ((stage_available * phase_a_percent) / 100)
                .max(min_turn_nodes.min(stage_available))
                .min(stage_available)
        } else {
            stage_available
        };
        let phase_a_time_ms = if avail_ms_before_a == 0 {
            0
        } else if reserve_for_paid_potions && avail_ms_before_a >= 400 {
            ((avail_ms_before_a * phase_a_time_percent as u64) / 100)
                .max(150)
                .min(avail_ms_before_a)
        } else {
            avail_ms_before_a
        };

        let mut phase_a_profile = base_profile.clone();
        phase_a_profile.beam_width = stage_width.clamp(1, MAX_BEAM_WIDTH);
        phase_a_profile.max_expanded_nodes = phase_a_nodes.max(1);
        phase_a_profile.soft_time_budget_ms = phase_a_time_ms;

        let phase_a = run_beam_pass_with_resources(
            root,
            content,
            policy,
            &phase_a_profile,
            cert,
            incumbent_table,
            shared_incumbent,
            pool,
            cancel,
            abort,
            progress_cb,
            PassSearchOptions {
                allow_paid_optional_potions: phase_a_allow_paid,
                prefer_potion_buckets: phase_a_allow_paid
                    && root_baseline_info.paid_optional_potion_count > 0,
                eval_mode,
                early_turn_scout_depth: 0,
                boss_tempo_scout_turns: 0,
                rollout_horizon_turns,
                allow_death_save_rescue,
                relax_zero_potion_reserve: false,
                min_zero_potion_reserve_percent: 0,
            },
            None,
            resources,
        )?;

        *total_expanded += phase_a.expanded_nodes;
        *total_transitions += phase_a.transition_count;
        *total_choice_replays += phase_a.choice_replay_attempts;
        *total_tt_pruned += phase_a.tt_pruned;
        *total_incumbent_pruned += phase_a.incumbent_pruned;
        *total_por_pruned += phase_a.por_pruned;

        if stage_number == 1 {
            *stage1_phase_a_completed_cleanly = phase_a.termination == "None";
            *stage1_phase_a_losslessly_exhausted = phase_a.was_losslessly_exhausted;
        }
        phase_a_was_losslessly_exhausted = phase_a.was_losslessly_exhausted;
        phase_a_boundary_reason = phase_a.termination;

        let phase_a_is_zero_baseline = phase_a.optional_paid_potion_count == 0;
        let take_a = match selected_best.as_ref() {
            None => true,
            Some(cur) => is_better_run_output_with_policy(
                &phase_a,
                cur,
                Some(policy),
                baseline_zero_potion.as_ref(),
            ),
        };
        let update_zero = phase_a_is_zero_baseline
            && baseline_zero_potion.as_ref().is_none_or(|cur_z| {
                is_better_run_output_with_policy(&phase_a, cur_z, Some(policy), None)
            });
        if take_a && update_zero {
            let summary_clone = phase_a.clone_summary();
            *selected_best = Some(phase_a);
            *baseline_zero_potion = Some(summary_clone);
            *post_best_boundary = "None";
        } else if take_a {
            *selected_best = Some(phase_a);
            *post_best_boundary = "None";
        } else {
            if phase_a_boundary_reason == "TimeLimit" {
                *post_best_boundary = "TimeLimit";
            } else if phase_a_boundary_reason == BeamWidthPortfolio::NODE_LIMIT_TERMINATION
                && *post_best_boundary != "TimeLimit"
            {
                *post_best_boundary = BeamWidthPortfolio::NODE_LIMIT_TERMINATION;
            }
            if update_zero {
                *baseline_zero_potion = Some(phase_a);
            }
        }
    } else {
        phase_a_was_losslessly_exhausted = true;
        phase_a_boundary_reason = "None";
    }

    let best_pass_unimprovable_paid_win = is_unimprovable_paid_win(
        stage_number,
        selected_best.as_ref(),
        root_baseline_info,
        check_theft,
    );
    let smart_needs_phase_b = smart_mode
        && !best_pass_unimprovable_paid_win
        && should_run_smart_potion_pass(
            baseline_zero_potion.as_ref(),
            root_baseline_info,
            check_theft,
        );
    let has_clean_survival_best = selected_best
        .as_ref()
        .is_some_and(|b| b.won && b.survives && b.death_save_use_count == 0);

    let mut phase_b_deadline_ms = stage_deadline_ms;
    if stage_number == 2
        && smart_needs_phase_b
        && can_scale_adaptive_stages
        && has_clean_survival_best
        && (*stage1_phase_b_timed_out
            || selected_best.as_ref().is_some_and(|b| {
                projected_battle_hp_lost_from_cumulative(root_baseline_info, b.hp_lost) >= 20
            }))
        && !cancel.load(Ordering::Relaxed)
        && full_deadline_ms > 0
    {
        if let Some(full_rem) = rem_ms_for(full_deadline_ms) {
            if full_rem >= 800 {
                let share_pct = if *stage1_phase_a_completed_cleanly {
                    85
                } else {
                    80
                };
                let elapsed_now = started.elapsed().as_millis() as u64;
                let extended = full_deadline_ms
                    .min(elapsed_now + 400u64.max((full_rem * share_pct) / 100));
                if extended > stage_deadline_ms {
                    phase_b_deadline_ms = extended;
                }
            }
        }
    }

    let phase_b_rem_ms = rem_ms_for(phase_b_deadline_ms);
    let run_paid_potion_phase = smart_needs_phase_b
        && !cancel.load(Ordering::Relaxed)
        && phase_b_rem_ms.is_some()
        && *total_expanded < stage_node_ceiling;

    if !run_paid_potion_phase {
        if smart_needs_phase_b {
            if phase_b_rem_ms.is_none() {
                *post_best_boundary = "TimeLimit";
            } else if *total_expanded >= stage_node_ceiling && *post_best_boundary != "TimeLimit" {
                *post_best_boundary = BeamWidthPortfolio::NODE_LIMIT_TERMINATION;
            }
        } else if stage_number > 1
            && !matches!(phase_a_boundary_reason, "TimeLimit" | "NodeLimit")
        {
            *post_best_boundary = "None";
        }
        return Ok(phase_a_was_losslessly_exhausted && !smart_needs_phase_b);
    }

    let avail_ms_b = phase_b_rem_ms.expect("checked above");
    let phase_b_nodes = stage_node_ceiling.saturating_sub(*total_expanded).max(1);
    let potion_width = ((stage_width * 5) / 4).max(stage_width).min(MAX_BEAM_WIDTH);
    let mut phase_b_profile = base_profile.clone();
    phase_b_profile.beam_width = potion_width;
    phase_b_profile.max_expanded_nodes = phase_b_nodes;
    phase_b_profile.soft_time_budget_ms = avail_ms_b;

    let phase_a_has_clean_win = baseline_zero_potion
        .as_ref()
        .is_some_and(|b| b.won && b.survives && b.death_save_use_count == 0);
    let phase_b_relax_zero_potion_reserve = !phase_a_has_clean_win;
    let phase_b_min_zero_potion_reserve_percent = if phase_a_has_clean_win { 0 } else { 25 };
    let phase_b_allow_death_save_rescue = allow_death_save_rescue && !has_clean_survival_best;

    let phase_b = run_beam_pass_with_resources(
        root,
        content,
        policy,
        &phase_b_profile,
        cert,
        incumbent_table,
        shared_incumbent,
        pool,
        cancel,
        abort,
        progress_cb,
        PassSearchOptions {
            allow_paid_optional_potions: true,
            prefer_potion_buckets: true,
            eval_mode,
            early_turn_scout_depth: 0,
            boss_tempo_scout_turns: 0,
            rollout_horizon_turns,
            allow_death_save_rescue: phase_b_allow_death_save_rescue,
            relax_zero_potion_reserve: phase_b_relax_zero_potion_reserve,
            min_zero_potion_reserve_percent: phase_b_min_zero_potion_reserve_percent,
        },
        None,
        resources,
    )?;

    *total_expanded += phase_b.expanded_nodes;
    *total_transitions += phase_b.transition_count;
    *total_choice_replays += phase_b.choice_replay_attempts;
    *total_tt_pruned += phase_b.tt_pruned;
    *total_incumbent_pruned += phase_b.incumbent_pruned;
    *total_por_pruned += phase_b.por_pruned;

    if stage_number == 1 && phase_b.termination == "TimeLimit" {
        *stage1_phase_b_timed_out = true;
    }

    let phase_b_exhausted = phase_b.was_losslessly_exhausted;
    let phase_b_term = phase_b.termination;
    let take_b = match selected_best.as_ref() {
        None => true,
        Some(cur) => is_better_run_output_with_policy(
            &phase_b,
            cur,
            Some(policy),
            baseline_zero_potion.as_ref(),
        ),
    };
    if take_b {
        *selected_best = Some(phase_b);
        *post_best_boundary = "None";
    } else {
        if phase_b.optional_paid_potion_count == 0
            && baseline_zero_potion.as_ref().is_none_or(|z| {
                is_better_run_output_with_policy(&phase_b, z, Some(policy), None)
            })
        {
            *baseline_zero_potion = Some(phase_b);
        }
        if phase_b_term == "TimeLimit" {
            *post_best_boundary = "TimeLimit";
        } else if phase_b_term == BeamWidthPortfolio::NODE_LIMIT_TERMINATION
            && *post_best_boundary != "TimeLimit"
        {
            *post_best_boundary = BeamWidthPortfolio::NODE_LIMIT_TERMINATION;
        } else if stage_number > 1
            && !matches!(phase_a_boundary_reason, "TimeLimit" | "NodeLimit")
            && !matches!(phase_b_term, "TimeLimit" | "NodeLimit")
        {
            *post_best_boundary = "None";
        }
    }

    Ok(phase_a_was_losslessly_exhausted && phase_b_exhausted)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AdaptiveStagePlan {
    is_default_adaptive_profile: bool,
    can_scale_custom_adaptive: bool,
    can_scale_adaptive_stages: bool,
    stage1_width: usize,
    stage1_ceiling: usize,
    stage2_width: usize,
    stage2_ceiling: usize,
    stage3_width: usize,
    stage3b_width: usize,
    stage3_ceiling: usize,
    max_total_nodes: usize,
}

impl AdaptiveStagePlan {
    fn compute(
        profile: &SolverSearchProfile,
        root_baseline: &RootEvalBaseline,
        adaptive_escalation_enabled: bool,
    ) -> Self {
        let is_default_adaptive_profile = adaptive_escalation_enabled
            && profile.beam_width == 96
            && profile.max_expanded_nodes == 150_000;
        let configured_beam = resolve_initial_beam_width(profile.beam_width, root_baseline);
        let configured_nodes = profile.max_expanded_nodes.max(1);
        let can_scale_custom_adaptive = adaptive_escalation_enabled
            && !is_default_adaptive_profile
            && configured_nodes >= 30_000;
        let can_scale_adaptive_stages = is_default_adaptive_profile || can_scale_custom_adaptive;
        let min_turn_nodes = crate::eval::weights::MINIMUM_TURN_LAYER_EXPANDED_NODES;

        let stage1_width = if is_default_adaptive_profile {
            96
        } else {
            configured_beam
        };
        let stage1_ceiling = if is_default_adaptive_profile {
            150_000
        } else if can_scale_custom_adaptive {
            ((configured_nodes * 35) / 100).max(min_turn_nodes)
        } else {
            configured_nodes
        };
        let stage2_width = if is_default_adaptive_profile {
            240
        } else {
            (configured_beam * 2).min(MAX_BEAM_WIDTH).max(configured_beam)
        };
        let stage2_ceiling = if is_default_adaptive_profile {
            320_000
        } else if can_scale_custom_adaptive {
            configured_nodes.min(
                (stage1_ceiling + min_turn_nodes).max((configured_nodes * 68) / 100),
            )
        } else {
            configured_nodes
        };
        let stage3_width = if is_default_adaptive_profile {
            320
        } else {
            ((stage2_width * 4) / 3).min(MAX_BEAM_WIDTH).max(stage2_width)
        };
        let stage3b_width = if is_default_adaptive_profile {
            384
        } else {
            ((stage3_width * 6) / 5).min(MAX_BEAM_WIDTH).max(stage3_width)
        };
        let stage3_ceiling = if is_default_adaptive_profile {
            500_000
        } else {
            configured_nodes
        };
        let max_total_nodes = if is_default_adaptive_profile {
            stage3_ceiling.max(650_000)
        } else {
            configured_nodes
        };

        Self {
            is_default_adaptive_profile,
            can_scale_custom_adaptive,
            can_scale_adaptive_stages,
            stage1_width,
            stage1_ceiling,
            stage2_width,
            stage2_ceiling,
            stage3_width,
            stage3b_width,
            stage3_ceiling,
            max_total_nodes,
        }
    }
}

#[inline]
fn should_stage1_early_exit(
    root_baseline: &RootEvalBaseline,
    policy: &SearchPolicy,
    stage1_out: &BeamRunOutput,
    stage1_losslessly_exhausted: bool,
    smart_mode: bool,
    check_theft: bool,
) -> bool {
    if stage1_losslessly_exhausted {
        return true;
    }
    let stage1_clean_survival = stage1_out.won
        && stage1_out.survives
        && stage1_out.death_save_use_count == 0
        && (!check_theft || (stage1_out.outstanding_stolen == 0 && !stage1_out.forfeits_gold));
    let stage1_raw_hp_lost =
        projected_battle_hp_lost_from_cumulative(root_baseline, stage1_out.hp_lost);
    let stage1_paid = stage1_out.optional_paid_potion_count;
    let can_improve = smart_mode
        && can_any_paid_potion_improve_output(root_baseline, stage1_out, check_theft);
    let stage1_meets_target =
        has_reached_acceptable_battle_hp_loss(root_baseline, policy, stage1_out) && !can_improve;
    stage1_meets_target
        || (stage1_clean_survival
            && are_strategy_targets_satisfied_output(root_baseline, stage1_out)
            && satisfies_run_output_hard_constraints(stage1_out)
            && stage1_raw_hp_lost <= 0
            && stage1_paid == 0
            && !can_improve)
}

#[inline]
fn should_escalate_to_stage3(
    root_baseline: &RootEvalBaseline,
    policy: &SearchPolicy,
    stage2_out: &BeamRunOutput,
    stage2_losslessly_exhausted: bool,
    is_elite_or_boss: bool,
    smart_mode: bool,
    check_theft: bool,
) -> bool {
    if stage2_losslessly_exhausted {
        return false;
    }
    let has_any_strategy_targets = root_baseline.effective_has_growth_targets
        || !root_baseline.active_relic_targets.is_empty();
    let stage2_clean_survival = stage2_out.won
        && stage2_out.survives
        && stage2_out.death_save_use_count == 0
        && (!check_theft || (stage2_out.outstanding_stolen == 0 && !stage2_out.forfeits_gold));
    let stage2_eff_loss = stage2_out.strategic_hp_deficit.max(0);
    let stage2_raw_hp_lost =
        projected_battle_hp_lost_from_cumulative(root_baseline, stage2_out.hp_lost);
    let stage2_trigger_hp_loss = stage2_eff_loss.max(stage2_raw_hp_lost);
    let stage2_paid = stage2_out.optional_paid_potion_count;
    let stage2_all_strategy_sat =
        are_strategy_targets_satisfied_output(root_baseline, stage2_out);
    let root_hp = root_baseline.initial_player_hp.max(1);
    let can_improve = smart_mode
        && can_any_paid_potion_improve_output(root_baseline, stage2_out, check_theft);
    let stage2_meets_target =
        has_reached_acceptable_battle_hp_loss(root_baseline, policy, stage2_out) && !can_improve;

    !stage2_meets_target
        && (!stage2_out.won
            || !stage2_clean_survival
            || (has_any_strategy_targets && !stage2_all_strategy_sat)
            || stage2_trigger_hp_loss >= 18
            || stage2_trigger_hp_loss * 4 >= root_hp
            || (is_elite_or_boss && (stage2_trigger_hp_loss >= 8 || stage2_paid >= 1)))
}

#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn solve_adaptive_stages(
    root: &BattleState,
    content: &(dyn ContentDispatcher + Sync),
    policy: &SearchPolicy,
    profile: &SolverSearchProfile,
    cert: &ClosureCertificate,
    incumbent_table: &PrimaryIncumbentTable,
    pool: Option<&rayon::ThreadPool>,
    cancel: &AtomicBool,
    abort: &SearchAbortSlot,
    progress_cb: Option<&(dyn Fn(&SearchProgress) + Sync)>,
    adaptive_escalation_enabled: bool,
    total_expanded: &mut usize,
    total_transitions: &mut u64,
    total_choice_replays: &mut u64,
    total_tt_pruned: &mut u64,
    total_incumbent_pruned: &mut u64,
    total_por_pruned: &mut u64,
    resources: &mut PassResources,
) -> Result<BeamRunOutput, KernelError> {
    let started = Instant::now();
    let root_baseline_info = RootEvalBaseline::from_state_with_policy(root, content, policy);
    let plan = AdaptiveStagePlan::compute(profile, &root_baseline_info, adaptive_escalation_enabled);
    let AdaptiveStagePlan {
        can_scale_adaptive_stages,
        stage1_width,
        stage1_ceiling,
        stage2_width,
        stage2_ceiling,
        stage3_width,
        stage3b_width,
        stage3_ceiling,
        max_total_nodes,
        ..
    } = plan;

    let is_elite_or_boss = matches!(root.static_ctx.room_type, RoomType::Elite | RoomType::Boss);
    let smart_mode = root_baseline_info.effective_potion_policy == SolverPotionPolicy::Smart;
    let check_theft = policy.theft_policy == SolverTheftPolicy::PreserveResources;
    let min_turn_nodes = crate::eval::weights::MINIMUM_TURN_LAYER_EXPANDED_NODES;

    let mut shared_incumbent = FrozenIncumbentBound::default();
    if cancel.load(Ordering::Relaxed) {
        return run_beam_pass_with_resources(
            root,
            content,
            policy,
            profile,
            cert,
            incumbent_table,
            &mut shared_incumbent,
            pool,
            cancel,
            abort,
            progress_cb,
            PassSearchOptions::default(),
            None,
            resources,
        );
    }

    let mut baseline_zero_potion: Option<BeamRunOutput> = None;
    let mut selected_best: Option<BeamRunOutput> = None;
    let mut post_best_boundary: &'static str = "None";
    let mut stage1_phase_b_timed_out = false;
    let mut stage1_phase_a_completed_cleanly = false;
    let mut stage1_phase_a_losslessly_exhausted = false;

    let full_deadline_ms = profile.soft_time_budget_ms;
    let stage1_deadline_ms = if can_scale_adaptive_stages && full_deadline_ms >= 1500 {
        ((full_deadline_ms * 50) / 100).clamp(600, full_deadline_ms)
    } else {
        full_deadline_ms
    };
    let stage1_rollout_horizon = if is_elite_or_boss { 6 } else { 4 };

    let stage1_losslessly_exhausted = run_adaptive_stage(
        root,
        content,
        policy,
        profile,
        &root_baseline_info,
        cert,
        incumbent_table,
        &mut shared_incumbent,
        pool,
        cancel,
        abort,
        progress_cb,
        &started,
        1,
        stage1_width,
        stage1_ceiling,
        EvalMode::Balanced,
        stage1_rollout_horizon,
        stage1_deadline_ms,
        full_deadline_ms,
        can_scale_adaptive_stages,
        false,
        &mut stage1_phase_b_timed_out,
        &mut stage1_phase_a_completed_cleanly,
        &mut stage1_phase_a_losslessly_exhausted,
        &mut post_best_boundary,
        total_expanded,
        total_transitions,
        total_choice_replays,
        total_tt_pruned,
        total_incumbent_pruned,
        total_por_pruned,
        &mut baseline_zero_potion,
        &mut selected_best,
        resources,
    )?;

    let Some(stage1_out) = selected_best.as_ref() else {
        return run_beam_pass_with_resources(
            root,
            content,
            policy,
            profile,
            cert,
            incumbent_table,
            &mut shared_incumbent,
            pool,
            cancel,
            abort,
            progress_cb,
            PassSearchOptions::default(),
            None,
            resources,
        );
    };
    let stage1_early_exit = should_stage1_early_exit(
        &root_baseline_info,
        policy,
        stage1_out,
        stage1_losslessly_exhausted,
        smart_mode,
        check_theft,
    );

    let has_time_left = full_deadline_ms == 0
        || (started.elapsed().as_millis() as u64) < full_deadline_ms;
    if can_scale_adaptive_stages
        && !stage1_early_exit
        && !cancel.load(Ordering::Relaxed)
        && has_time_left
        && *total_expanded < stage2_ceiling
    {
        let stage2_deadline_ms = if full_deadline_ms >= 1500 {
            let elapsed_now = started.elapsed().as_millis() as u64;
            let rem = full_deadline_ms.saturating_sub(elapsed_now);
            if rem >= 900 {
                let stage3_reserved = (full_deadline_ms * 20) / 100;
                let min_stage2_slice = rem.min(400u64.max((rem * 50) / 100));
                let target_deadline = full_deadline_ms.saturating_sub(stage3_reserved);
                let min_deadline = full_deadline_ms.min(elapsed_now + min_stage2_slice);
                target_deadline.clamp(min_deadline, full_deadline_ms)
            } else {
                full_deadline_ms
            }
        } else {
            full_deadline_ms
        };

        let stage2_losslessly_exhausted = run_adaptive_stage(
            root,
            content,
            policy,
            profile,
            &root_baseline_info,
            cert,
            incumbent_table,
            &mut shared_incumbent,
            pool,
            cancel,
            abort,
            progress_cb,
            &started,
            2,
            stage2_width,
            stage2_ceiling,
            EvalMode::SetupAndSurvival,
            6,
            stage2_deadline_ms,
            full_deadline_ms,
            can_scale_adaptive_stages,
            false,
            &mut stage1_phase_b_timed_out,
            &mut stage1_phase_a_completed_cleanly,
            &mut stage1_phase_a_losslessly_exhausted,
            &mut post_best_boundary,
            total_expanded,
            total_transitions,
            total_choice_replays,
            total_tt_pruned,
            total_incumbent_pruned,
            total_por_pruned,
            &mut baseline_zero_potion,
            &mut selected_best,
            resources,
        )?;

        let stage2_out = selected_best.as_ref().expect("Stage 2 produced no output");
        let stage2_clean_survival = stage2_out.won
            && stage2_out.survives
            && stage2_out.death_save_use_count == 0
            && (!check_theft
                || (stage2_out.outstanding_stolen == 0 && !stage2_out.forfeits_gold));
        let has_time_for_3 = full_deadline_ms == 0
            || (started.elapsed().as_millis() as u64) < full_deadline_ms;
        let need_stage3 = can_scale_adaptive_stages
            && !cancel.load(Ordering::Relaxed)
            && has_time_for_3
            && *total_expanded < stage3_ceiling
            && should_escalate_to_stage3(
                &root_baseline_info,
                policy,
                stage2_out,
                stage2_losslessly_exhausted,
                is_elite_or_boss,
                smart_mode,
                check_theft,
            );

        if need_stage3 {
            if stage2_out.won && stage2_clean_survival {
                let _ = run_adaptive_stage(
                    root,
                    content,
                    policy,
                    profile,
                    &root_baseline_info,
                    cert,
                    incumbent_table,
                    &mut shared_incumbent,
                    pool,
                    cancel,
                    abort,
                    progress_cb,
                    &started,
                    3,
                    stage3_width,
                    stage3_ceiling,
                    EvalMode::Balanced,
                    6,
                    full_deadline_ms,
                    full_deadline_ms,
                    can_scale_adaptive_stages,
                    false,
                    &mut stage1_phase_b_timed_out,
                    &mut stage1_phase_a_completed_cleanly,
                    &mut stage1_phase_a_losslessly_exhausted,
                    &mut post_best_boundary,
                    total_expanded,
                    total_transitions,
                    total_choice_replays,
                    total_tt_pruned,
                    total_incumbent_pruned,
                    total_por_pruned,
                    &mut baseline_zero_potion,
                    &mut selected_best,
                    resources,
                )?;
            } else {
                let stage3_rem_nodes = stage3_ceiling.saturating_sub(*total_expanded).max(1);
                let stage3a_alloc_nodes = ((stage3_rem_nodes * 55) / 100).max(min_turn_nodes);
                let stage3a_ceiling = stage3_ceiling.min(*total_expanded + stage3a_alloc_nodes);
                let stage3a_deadline_ms = if full_deadline_ms > 0 {
                    let elapsed_now = started.elapsed().as_millis() as u64;
                    let rem = full_deadline_ms.saturating_sub(elapsed_now);
                    if rem >= 1200 {
                        let alloc = 300u64.max((rem * 55) / 100);
                        full_deadline_ms.min(elapsed_now + alloc)
                    } else {
                        full_deadline_ms
                    }
                } else {
                    0
                };
                let stage3a_allow_ds = !stage2_out.won;

                let _ = run_adaptive_stage(
                    root,
                    content,
                    policy,
                    profile,
                    &root_baseline_info,
                    cert,
                    incumbent_table,
                    &mut shared_incumbent,
                    pool,
                    cancel,
                    abort,
                    progress_cb,
                    &started,
                    3,
                    stage3_width,
                    stage3a_ceiling,
                    EvalMode::Offense,
                    6,
                    stage3a_deadline_ms,
                    full_deadline_ms,
                    can_scale_adaptive_stages,
                    stage3a_allow_ds,
                    &mut stage1_phase_b_timed_out,
                    &mut stage1_phase_a_completed_cleanly,
                    &mut stage1_phase_a_losslessly_exhausted,
                    &mut post_best_boundary,
                    total_expanded,
                    total_transitions,
                    total_choice_replays,
                    total_tt_pruned,
                    total_incumbent_pruned,
                    total_por_pruned,
                    &mut baseline_zero_potion,
                    &mut selected_best,
                    resources,
                )?;

                let cur_best = selected_best.as_ref().expect("Stage 3a produced no output");
                let stage3a_clean_survival =
                    cur_best.won && cur_best.survives && cur_best.death_save_use_count == 0;
                let rem_ok_for_3b = full_deadline_ms == 0
                    || full_deadline_ms.saturating_sub(started.elapsed().as_millis() as u64)
                        >= 400;
                if !stage3a_clean_survival
                    && (!cur_best.won || cur_best.death_save_use_count > 0)
                    && !cancel.load(Ordering::Relaxed)
                    && rem_ok_for_3b
                {
                    let stage3b_ceiling = if !cur_best.won {
                        max_total_nodes.min(stage3_ceiling.max(650_000))
                    } else {
                        stage3_ceiling
                    };
                    let stage3b_allow_ds = !cur_best.won;
                    if *total_expanded < stage3b_ceiling {
                        let _ = run_adaptive_stage(
                            root,
                            content,
                            policy,
                            profile,
                            &root_baseline_info,
                            cert,
                            incumbent_table,
                            &mut shared_incumbent,
                            pool,
                            cancel,
                            abort,
                            progress_cb,
                            &started,
                            3,
                            stage3b_width,
                            stage3b_ceiling,
                            EvalMode::DesperateRescue,
                            6,
                            full_deadline_ms,
                            full_deadline_ms,
                            can_scale_adaptive_stages,
                            stage3b_allow_ds,
                            &mut stage1_phase_b_timed_out,
                            &mut stage1_phase_a_completed_cleanly,
                            &mut stage1_phase_a_losslessly_exhausted,
                            &mut post_best_boundary,
                            total_expanded,
                            total_transitions,
                            total_choice_replays,
                            total_tt_pruned,
                            total_incumbent_pruned,
                            total_por_pruned,
                            &mut baseline_zero_potion,
                            &mut selected_best,
                            resources,
                        )?;
                    }
                }
            }
        }
    }

    let mut out = selected_best.expect("adaptive stages produced no output");
    let reached_global_stop_target = out.won
        && has_reached_acceptable_battle_hp_loss(&root_baseline_info, policy, &out)
        && (!smart_mode
            || !can_any_paid_potion_improve_output(&root_baseline_info, &out, check_theft));
    let effective_final_node_ceiling = if out.won {
        stage3_ceiling
    } else {
        max_total_nodes
    };
    let overall_time_expired = !reached_global_stop_target
        && full_deadline_ms > 0
        && (started.elapsed().as_millis() as u64) >= full_deadline_ms;
    let overall_node_limit_reached =
        !reached_global_stop_target && *total_expanded >= effective_final_node_ceiling;
    if !out.is_event_defeat && out.termination == "None" {
        if overall_time_expired || post_best_boundary == "TimeLimit" {
            out.termination = "TimeLimit";
        } else if overall_node_limit_reached
            || post_best_boundary == BeamWidthPortfolio::NODE_LIMIT_TERMINATION
        {
            out.termination = BeamWidthPortfolio::NODE_LIMIT_TERMINATION;
        }
    }
    Ok(out)
}

#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn solve_beam_portfolio_orchestration(
    root: &BattleState,
    content: &(dyn ContentDispatcher + Sync),
    policy: &SearchPolicy,
    profile: &SolverSearchProfile,
    cert: &ClosureCertificate,
    incumbent_table: &PrimaryIncumbentTable,
    pool: Option<&rayon::ThreadPool>,
    cancel: &AtomicBool,
    abort: &SearchAbortSlot,
    progress_cb: Option<&(dyn Fn(&SearchProgress) + Sync)>,
    total_expanded: &mut usize,
    total_transitions: &mut u64,
    total_choice_replays: &mut u64,
    total_tt_pruned: &mut u64,
    total_incumbent_pruned: &mut u64,
    total_por_pruned: &mut u64,
    resources: &mut PassResources,
) -> Result<BeamRunOutput, KernelError> {
    let started = Instant::now();
    let has_power = PowerCardValuationRegistry::has_reachable_power(root);
    let specs = BeamWidthPortfolio::production_members(
        profile.beam_width,
        policy.beam_width_portfolio_widths.as_deref(),
        policy.beam_width_portfolio_plain_baseline_member,
        has_power,
        policy.offensive_refinement_portfolio,
        policy.bounded_offensive_refinement_portfolio,
    );
    let baseline_info: parking_lot::Mutex<Option<BeamWidthPortfolioBaseline>> =
        parking_lot::Mutex::new(None);
    let best_incumbent_target_hit: parking_lot::Mutex<bool> = parking_lot::Mutex::new(false);
    let pass_error: parking_lot::Mutex<Option<KernelError>> = parking_lot::Mutex::new(None);

    let record_member_res = |beam_width: usize,
                             m_start: Instant,
                             res: Result<BeamRunOutput, KernelError>| {
        match res {
            Ok(out) => {
                {
                    let mut bi = baseline_info.lock();
                    if bi.is_none() {
                        *bi = Some(BeamWidthPortfolioBaseline {
                            frontier_exhausted: out.termination == "None"
                                || out.termination == "HpTarget",
                            proven_zero_damage: out.won
                                && out.hp_lost == 0
                                && out.potion_count == 0
                                && cert.healing_certified,
                            elapsed_ms: m_start.elapsed().as_millis() as u64,
                            expanded_nodes: out.expanded_nodes,
                            beam_width,
                        });
                    }
                }
                if can_finish_target_portfolio(&out.root_baseline, policy, profile, &out, cert) {
                    *best_incumbent_target_hit.lock() = true;
                }
                let expanded = out.expanded_nodes;
                let trans = out.transition_count;
                let term = out.termination;
                let won = out.won;
                let hp_lost = out.hp_lost;
                let potions = out.potion_count;
                BeamWidthPortfolioRun {
                    result: Some(out),
                    expanded_nodes: expanded,
                    transition_count: trans,
                    termination: term,
                    terminal: won,
                    won,
                    battle_hp_lost: Some(hp_lost),
                    potion_count: potions,
                    stop_portfolio: cancel.load(Ordering::Relaxed) || abort.is_tripped(),
                    memory_truncated: false,
                }
            }
            Err(e) => {
                let mut pe = pass_error.lock();
                if pe.is_none() {
                    *pe = Some(e);
                }
                BeamWidthPortfolioRun {
                    result: None,
                    expanded_nodes: 0,
                    transition_count: 0,
                    termination: "Error",
                    terminal: false,
                    won: false,
                    battle_hp_lost: None,
                    potion_count: 0,
                    stop_portfolio: true,
                    memory_truncated: false,
                }
            }
        }
    };

    let reject_fn = |spec: &crate::portfolio::BeamWidthPortfolioMemberSpec| {
        let bl = *baseline_info.lock();
        let Some(bl) = bl else {
            return None;
        };
        if *best_incumbent_target_hit.lock() {
            return Some("AcceptableBattleHpLoss");
        }
        if spec.aggressive_power_commitment {
            return PowerCommitmentPortfolioGate::reject(has_power);
        }
        let remaining_ms = profile
            .soft_time_budget_ms
            .saturating_sub(started.elapsed().as_millis() as u64);
        if profile.soft_time_budget_ms == 0 {
            return None;
        }
        BeamWidthPortfolioGate::reject_refinement(
            &bl,
            spec.beam_width,
            profile.max_expanded_nodes.saturating_sub(bl.expanded_nodes),
            remaining_ms,
            profile.soft_time_budget_ms,
        )
    };

    let compare_fn = |cand: &Option<BeamRunOutput>, cur: &Option<BeamRunOutput>| match (cand, cur) {
        (Some(a), Some(b)) => is_better_run_output(a, b),
        (Some(_), None) => true,
        _ => false,
    };

    let dop = policy
        .max_degree_of_parallelism
        .clamp(1, crate::eval::weights::MAXIMUM_SEARCH_MAX_DEGREE_OF_PARALLELISM);
    let outcome = if policy.portfolio_scheduling_mode == PortfolioSchedulingMode::Parallel && dop > 1
    {
        BeamWidthPortfolio::run_with_mode(
            &specs,
            profile.max_expanded_nodes,
            profile,
            policy.portfolio_scheduling_mode,
            dop,
            |member_profile| {
                let m_start = Instant::now();
                let mut member_incumbent = FrozenIncumbentBound::default();
                let res = run_beam_pass(
                    root,
                    content,
                    policy,
                    member_profile,
                    cert,
                    incumbent_table,
                    &mut member_incumbent,
                    pool,
                    cancel,
                    abort,
                    None,
                    PassSearchOptions::default(),
                    None,
                );
                record_member_res(member_profile.beam_width, m_start, res)
            },
            compare_fn,
            reject_fn,
        )
    } else {
        BeamWidthPortfolio::run(
            &specs,
            profile.max_expanded_nodes,
            profile,
            |member_profile| {
                let m_start = Instant::now();
                let mut member_incumbent = FrozenIncumbentBound::default();
                let res = run_beam_pass_with_resources(
                    root,
                    content,
                    policy,
                    member_profile,
                    cert,
                    incumbent_table,
                    &mut member_incumbent,
                    pool,
                    cancel,
                    abort,
                    progress_cb,
                    PassSearchOptions::default(),
                    None,
                    resources,
                );
                record_member_res(member_profile.beam_width, m_start, res)
            },
            compare_fn,
            reject_fn,
        )
    };

    if let Some(err) = pass_error.into_inner().or_else(|| abort.clone_error()) {
        return Err(err);
    }
    let out = outcome.selected.expect("portfolio produced no output");
    *total_expanded += outcome.total_expanded_nodes;
    *total_transitions += outcome.total_transition_count;
    *total_choice_replays += out.choice_replay_attempts;
    *total_tt_pruned += out.tt_pruned;
    *total_incumbent_pruned += out.incumbent_pruned;
    *total_por_pruned += out.por_pruned;
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn solve_beam_or_portfolio(
    root: &BattleState,
    content: &(dyn ContentDispatcher + Sync),
    policy: &SearchPolicy,
    profile: &SolverSearchProfile,
    cert: &ClosureCertificate,
    incumbent_table: &PrimaryIncumbentTable,
    pool: Option<&rayon::ThreadPool>,
    cancel: &AtomicBool,
    abort: &SearchAbortSlot,
    progress_cb: Option<&(dyn Fn(&SearchProgress) + Sync)>,
    total_expanded: &mut usize,
    total_transitions: &mut u64,
    total_choice_replays: &mut u64,
    total_tt_pruned: &mut u64,
    total_incumbent_pruned: &mut u64,
    total_por_pruned: &mut u64,
    resources: &mut PassResources,
) -> Result<BeamRunOutput, KernelError> {
    let use_adaptive_stages = !policy.use_novelty_search
        && !policy.adaptive_novelty_refinement
        && policy
            .beam_width_portfolio_widths
            .as_ref()
            .is_none_or(|w| w.is_empty())
        && policy.beam_width_portfolio_plain_baseline_member
        && !policy.offensive_refinement_portfolio
        && !policy.bounded_offensive_refinement_portfolio
        && policy.portfolio_scheduling_mode == PortfolioSchedulingMode::Sequential;

    if !policy.use_beam_width_portfolio || use_adaptive_stages {
        return solve_adaptive_stages(
            root,
            content,
            policy,
            profile,
            cert,
            incumbent_table,
            pool,
            cancel,
            abort,
            progress_cb,
            policy.use_beam_width_portfolio,
            total_expanded,
            total_transitions,
            total_choice_replays,
            total_tt_pruned,
            total_incumbent_pruned,
            total_por_pruned,
            resources,
        );
    }

    solve_beam_portfolio_orchestration(
        root,
        content,
        policy,
        profile,
        cert,
        incumbent_table,
        pool,
        cancel,
        abort,
        progress_cb,
        total_expanded,
        total_transitions,
        total_choice_replays,
        total_tt_pruned,
        total_incumbent_pruned,
        total_por_pruned,
        resources,
    )
}

#[cold]
#[inline(never)]
fn prepare_compacted_root(root: &BattleState, content: &dyn ContentDispatcher) -> Option<BattleState> {
    let need_card_floors =
        root.static_ctx.root_card_floors.is_empty() && !root.cards.is_empty();
    let need_clutter_indices = !root.static_ctx.has_root_clutter_indices
        && !root.cards.is_empty()
        && root.cards.len() <= u16::MAX as usize;
    let need_initial_cards = root.static_ctx.root_combat_delta_len == 0
        && !root.cards.is_empty()
        && root.cards.len() <= 512
        && root.cards_are_contiguous();
    let need_card_hook_mask = (root.static_ctx.root_combat_delta_len > 0 || need_initial_cards)
        && !root.static_ctx.has_root_card_hook_mask
        && content.hook_mask_tables().is_some();
    let need_handle_layout = (!root.handle_layout.cards && root.cards_are_contiguous())
        || (!root.handle_layout.creatures && root.creatures_are_contiguous());
    let need_extra_clutter_clear = root.extra_clutter_hint.0
        && !root.cards.is_empty()
        && root.cards.len() <= u16::MAX as usize
        && (!root.static_ctx.has_root_clutter_indices
            || root.cards.len() == root.static_ctx.root_initial_cards_len);
    let need_turn_cache = !root.semantic_history.turn_cache.valid;
    let need_eot_clear = root.eot_card_cleanup_dirty.0
        && !root
            .cards
            .iter()
            .any(|c| BattleState::card_has_eot_cleanup_state(c, &root.card_extensions));

    if root.static_ctx.compact_turn_history
        && root.static_ctx.has_root_rngs
        && root.static_ctx.has_root_relics
        && !need_card_floors
        && !need_clutter_indices
        && !need_initial_cards
        && !need_card_hook_mask
        && !need_handle_layout
        && !need_extra_clutter_clear
        && !need_turn_cache
        && !need_eot_clear
    {
        return None;
    }

    let mut compacted_root = root.clone();
    if need_eot_clear {
        compacted_root.eot_card_cleanup_dirty = sts2_kernel::state::EotCleanupDirtyFlag(false);
    }
    let mut ctx = (*root.static_ctx).clone();
    ctx.compact_turn_history = true;
    if !ctx.has_root_rngs {
        ctx.set_root_rngs(root.run_rngs.streams, root.player_rngs.streams);
    }
    if !ctx.has_root_relics {
        ctx.set_root_relics(&root.player.relics);
        compacted_root.relic_dirty_mask = RelicDirtyMask(0);
    }
    if ctx.root_card_floors.is_empty() && !root.cards.is_empty() {
        ctx.set_root_card_floors(&root.cards);
    }
    if !ctx.has_root_clutter_indices
        && !root.cards.is_empty()
        && root.cards.len() <= u16::MAX as usize
    {
        if let Some(tables) = content.hook_mask_tables() {
            ctx.set_root_clutter_indices(&root.cards, |id| tables.card_is_status_or_curse(id));
        } else {
            ctx.set_root_clutter_indices(&root.cards, |id| {
                content.card_meta(id).is_ok_and(|m| {
                    matches!(
                        m.card_type,
                        sts2_types::CardType::Status | sts2_types::CardType::Curse
                    )
                })
            });
        }
    }
    if ctx.has_root_clutter_indices
        && compacted_root.cards.len() == ctx.root_initial_cards_len
    {
        compacted_root.extra_clutter_hint = sts2_kernel::state::ExtraClutterHintFlag(false);
    }
    ctx.root_deck_contiguous = ctx
        .root_deck_cards
        .iter()
        .enumerate()
        .all(|(i, c)| c.handle.0 == (i + 1) as u32);
    if ctx.root_combat_delta_len == 0 && !root.cards.is_empty() && root.cards.len() <= 512 {
        ctx.set_root_initial_cards(&root.cards, &root.player);
        if ctx.root_combat_delta_len > 0 {
            compacted_root.card_meta_dirty = sts2_kernel::state::CardMetaDirtyMask::CLEAN;
        }
    }
    if ctx.root_combat_delta_len > 0 && !ctx.has_root_card_hook_mask {
        if let Some(tables) = content.hook_mask_tables() {
            ctx.set_root_card_hook_mask(|c| {
                !c.removed_from_state
                    && (tables.card_has_hook(c.id)
                        || c.ext_index.is_some()
                        || (!tables.afflictions_self_card_entered_only && c.affliction.is_some()))
            });
        }
    }
    if !compacted_root.semantic_history.turn_cache.valid {
        let tk = compacted_root.current_turn_key();
        compacted_root.semantic_history.refresh_turn_cache(&tk);
    }
    compacted_root.recompute_handle_layout();
    compacted_root.static_ctx = Arc::new(ctx);
    Some(compacted_root)
}

#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn run_novelty_mode(
    root: &BattleState,
    content: &(dyn ContentDispatcher + Sync),
    policy: &SearchPolicy,
    base_profile: &SolverSearchProfile,
    cert: &ClosureCertificate,
    incumbent_table: &PrimaryIncumbentTable,
    pool_ref: Option<&rayon::ThreadPool>,
    cancel: &AtomicBool,
    abort: &SearchAbortSlot,
    progress_cb: Option<&(dyn Fn(&SearchProgress) + Sync)>,
    started: Instant,
    act_ending_boss: bool,
    total_expanded: &mut usize,
    total_transitions: &mut u64,
    total_choice_replays: &mut u64,
    total_tt_pruned: &mut u64,
    total_incumbent_pruned: &mut u64,
    total_por_pruned: &mut u64,
    resources: &mut PassResources,
) -> Result<BeamRunOutput, KernelError> {
    if let Some(exp_profile) = policy
        .novelty_budget
        .exploration(base_profile, act_ending_boss)
    {
        let exp_out = run_novelty_pass(
            root,
            content,
            policy,
            &exp_profile,
            cert,
            incumbent_table,
            cancel,
            abort,
            &policy.novelty_options,
        )?;
        *total_expanded += exp_out.expanded_nodes;
        *total_transitions += exp_out.transition_count;
        *total_choice_replays += exp_out.choice_replay_attempts;
        *total_tt_pruned += exp_out.tt_pruned;
        *total_incumbent_pruned += exp_out.incumbent_pruned;
        *total_por_pruned += exp_out.por_pruned;

        let settled = exp_out.root_baseline.all_forced_potion_mask == 0
            && has_reached_acceptable_battle_hp_loss(&exp_out.root_baseline, policy, &exp_out);
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let remaining = NoveltyPortfolioBudget::remaining(
            base_profile,
            elapsed_ms,
            exp_out.expanded_nodes,
        );
        if settled || remaining.is_none() || cancel.load(Ordering::Relaxed) {
            return Ok(exp_out);
        }
        let beam_out = solve_beam_or_portfolio(
            root,
            content,
            policy,
            &remaining.unwrap(),
            cert,
            incumbent_table,
            pool_ref,
            cancel,
            abort,
            progress_cb,
            total_expanded,
            total_transitions,
            total_choice_replays,
            total_tt_pruned,
            total_incumbent_pruned,
            total_por_pruned,
            resources,
        )?;
        Ok(if is_better_run_output(&exp_out, &beam_out) {
            exp_out
        } else {
            beam_out
        })
    } else {
        solve_beam_or_portfolio(
            root,
            content,
            policy,
            base_profile,
            cert,
            incumbent_table,
            pool_ref,
            cancel,
            abort,
            progress_cb,
            total_expanded,
            total_transitions,
            total_choice_replays,
            total_tt_pruned,
            total_incumbent_pruned,
            total_por_pruned,
            resources,
        )
    }
}

#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn run_adaptive_novelty_mode(
    root: &BattleState,
    content: &(dyn ContentDispatcher + Sync),
    policy: &SearchPolicy,
    base_profile: &SolverSearchProfile,
    cert: &ClosureCertificate,
    incumbent_table: &PrimaryIncumbentTable,
    pool_ref: Option<&rayon::ThreadPool>,
    cancel: &AtomicBool,
    abort: &SearchAbortSlot,
    progress_cb: Option<&(dyn Fn(&SearchProgress) + Sync)>,
    started: Instant,
    act_ending_boss: bool,
    total_expanded: &mut usize,
    total_transitions: &mut u64,
    total_choice_replays: &mut u64,
    total_tt_pruned: &mut u64,
    total_incumbent_pruned: &mut u64,
    total_por_pruned: &mut u64,
    resources: &mut PassResources,
) -> Result<BeamRunOutput, KernelError> {
    let b_start = Instant::now();
    let baseline = solve_beam_or_portfolio(
        root,
        content,
        policy,
        base_profile,
        cert,
        incumbent_table,
        pool_ref,
        cancel,
        abort,
        progress_cb,
        total_expanded,
        total_transitions,
        total_choice_replays,
        total_tt_pruned,
        total_incumbent_pruned,
        total_por_pruned,
        resources,
    )?;
    let baseline_ms = b_start.elapsed().as_millis() as u64;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    if cancel.load(Ordering::Relaxed)
        || can_finish_target_portfolio(
            &baseline.root_baseline,
            policy,
            base_profile,
            &baseline,
            cert,
        )
    {
        return Ok(baseline);
    }
    if let Some(ref_profile) = policy.novelty_budget.refinement_after_baseline(
        base_profile,
        baseline_ms,
        baseline.expanded_nodes,
        elapsed_ms,
        act_ending_boss,
    ) {
        let nov_out = run_novelty_pass(
            root,
            content,
            policy,
            &ref_profile,
            cert,
            incumbent_table,
            cancel,
            abort,
            &policy.novelty_options,
        )?;
        *total_expanded += nov_out.expanded_nodes;
        *total_transitions += nov_out.transition_count;
        *total_choice_replays += nov_out.choice_replay_attempts;
        *total_tt_pruned += nov_out.tt_pruned;
        *total_incumbent_pruned += nov_out.incumbent_pruned;
        *total_por_pruned += nov_out.por_pruned;
        Ok(if is_better_run_output(&nov_out, &baseline) {
            nov_out
        } else {
            baseline
        })
    } else {
        Ok(baseline)
    }
}

#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn run_early_turn_exploration_pass(
    root: &BattleState,
    content: &(dyn ContentDispatcher + Sync),
    policy: &SearchPolicy,
    base_profile: &SolverSearchProfile,
    cert: &ClosureCertificate,
    incumbent_table: &PrimaryIncumbentTable,
    pool_ref: Option<&rayon::ThreadPool>,
    cancel: &AtomicBool,
    abort: &SearchAbortSlot,
    started: Instant,
    etc_budget_ms: u64,
    total_expanded: &mut usize,
    total_transitions: &mut u64,
    total_choice_replays: &mut u64,
    selected: &mut BeamRunOutput,
    resources: &mut PassResources,
) -> Result<(), KernelError> {
    let etc_remaining_ms =
        || etc_budget_ms.saturating_sub(started.elapsed().as_millis() as u64);
    if etc_remaining_ms() == 0 {
        return Ok(());
    }
    let mut scout_profile = base_profile.clone();
    scout_profile.beam_width = (base_profile.beam_width * 4).clamp(64, 384);
    scout_profile.max_expanded_nodes = base_profile.max_expanded_nodes.min(150_000);
    scout_profile.soft_time_budget_ms = etc_remaining_ms().max(1);
    let mut etc_incumbent = FrozenIncumbentBound::default();
    match run_beam_pass_with_resources(
        root,
        content,
        policy,
        &scout_profile,
        cert,
        incumbent_table,
        &mut etc_incumbent,
        pool_ref,
        cancel,
        abort,
        None,
        PassSearchOptions {
            early_turn_scout_depth: policy.early_turn_exploration_depth,
            ..Default::default()
        },
        None,
        resources,
    ) {
        Ok(scout) => {
            *total_expanded += scout.expanded_nodes;
            *total_transitions += scout.transition_count;
            *total_choice_replays += scout.choice_replay_attempts;
            if is_better_run_output(&scout, selected) {
                *selected = scout;
            } else {
                let ordered = interleave_potion_frontiers(scout.early_turn_frontiers);
                let mut cont_budget = EARLY_TURN_EXPLORATION_NODE_BUDGET;
                for (rank, cand) in ordered
                    .into_iter()
                    .take(EARLY_TURN_EXPLORATION_MAX_RANK_LIMIT)
                    .enumerate()
                {
                    if rank >= EARLY_TURN_EXPLORATION_INITIAL_RANK_LIMIT && !selected.won {
                        break;
                    }
                    let rem_ms = etc_remaining_ms();
                    if cont_budget < 100 || rem_ms == 0 || cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    let _ = select_early_turn_continuation_incumbent(
                        incumbent_table.try_get(PrimaryIncumbentBucket {
                            potions: 0,
                            forfeits_gold: false,
                        }),
                        false,
                        selected.potion_count > 0,
                        cand.actions
                            .iter()
                            .any(|a| matches!(a, PlannedAction::UsePotion { .. })),
                    );
                    let mut cont_profile = base_profile.clone();
                    cont_profile.max_expanded_nodes =
                        cont_budget.min(base_profile.max_expanded_nodes);
                    cont_profile.soft_time_budget_ms = rem_ms;
                    match run_beam_pass_with_resources(
                        root,
                        content,
                        policy,
                        &cont_profile,
                        cert,
                        incumbent_table,
                        &mut etc_incumbent,
                        pool_ref,
                        cancel,
                        abort,
                        None,
                        PassSearchOptions::default(),
                        Some(&cand.actions),
                        resources,
                    ) {
                        Ok(cont) => {
                            cont_budget = cont_budget.saturating_sub(cont.expanded_nodes);
                            *total_expanded += cont.expanded_nodes;
                            *total_transitions += cont.transition_count;
                            *total_choice_replays += cont.choice_replay_attempts;
                            if is_better_run_output(&cont, selected) {
                                *selected = cont;
                            }
                        }
                        Err(err) if abort.is_tripped() || should_abort_search_on_kernel_error(&err) => {
                            return Err(err);
                        }
                        Err(_) => {}
                    }
                }
            }
        }
        Err(err) if abort.is_tripped() || should_abort_search_on_kernel_error(&err) => {
            return Err(err);
        }
        Err(_) => {}
    }
    Ok(())
}

#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn run_boss_tempo_post_pass(
    root: &BattleState,
    content: &(dyn ContentDispatcher + Sync),
    policy: &SearchPolicy,
    base_profile: &SolverSearchProfile,
    cert: &ClosureCertificate,
    incumbent_table: &PrimaryIncumbentTable,
    pool_ref: Option<&rayon::ThreadPool>,
    cancel: &AtomicBool,
    abort: &SearchAbortSlot,
    total_expanded: &mut usize,
    total_transitions: &mut u64,
    total_choice_replays: &mut u64,
    selected: &mut BeamRunOutput,
    resources: &mut PassResources,
) -> Result<(), KernelError> {
    let tempo_budget =
        BossTempoSearchOptions::additional_budget(base_profile, policy.time_budget_ms.max(1000));
    let mut tempo_incumbent = FrozenIncumbentBound::default();
    match run_beam_pass_with_resources(
        root,
        content,
        policy,
        &tempo_budget,
        cert,
        incumbent_table,
        &mut tempo_incumbent,
        pool_ref,
        cancel,
        abort,
        None,
        PassSearchOptions {
            boss_tempo_scout_turns: policy.boss_tempo_options.scout_turns.max(1),
            ..Default::default()
        },
        None,
        resources,
    ) {
        Ok(scout) => {
            *total_expanded += scout.expanded_nodes;
            *total_transitions += scout.transition_count;
            *total_choice_replays += scout.choice_replay_attempts;
            let prefixes = scout.boss_tempo_prefixes.clone();
            if is_better_run_output(&scout, selected) {
                *selected = scout;
            }
            for prefix in prefixes.into_iter().take(2) {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                match run_beam_pass_with_resources(
                    root,
                    content,
                    policy,
                    &tempo_budget,
                    cert,
                    incumbent_table,
                    &mut tempo_incumbent,
                    pool_ref,
                    cancel,
                    abort,
                    None,
                    PassSearchOptions::default(),
                    Some(&prefix.actions),
                    resources,
                ) {
                    Ok(cont) => {
                        *total_expanded += cont.expanded_nodes;
                        *total_transitions += cont.transition_count;
                        *total_choice_replays += cont.choice_replay_attempts;
                        if is_better_run_output(&cont, selected) {
                            *selected = cont;
                        }
                    }
                    Err(err) if abort.is_tripped() || should_abort_search_on_kernel_error(&err) => {
                        return Err(err);
                    }
                    Err(_) => {}
                }
            }
        }
        Err(err) if abort.is_tripped() || should_abort_search_on_kernel_error(&err) => {
            return Err(err);
        }
        Err(_) => {}
    }
    match run_boss_tempo_depth_first(
        root,
        content,
        policy,
        &tempo_budget,
        cert,
        incumbent_table,
        cancel,
        abort,
        &policy.boss_tempo_options,
    ) {
        Ok(tempo_out) => {
            *total_expanded += tempo_out.expanded_nodes;
            *total_transitions += tempo_out.transition_count;
            *total_choice_replays += tempo_out.choice_replay_attempts;
            if is_better_run_output(&tempo_out, selected) {
                *selected = tempo_out;
            }
        }
        Err(err) if abort.is_tripped() || should_abort_search_on_kernel_error(&err) => {
            return Err(err);
        }
        Err(_) => {}
    }
    Ok(())
}

#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn build_solve_response(
    selected: BeamRunOutput,
    cert: ClosureCertificate,
    incumbent_table: &PrimaryIncumbentTable,
    started: Instant,
    total_expanded: usize,
    total_transitions: u64,
    total_choice_replays: u64,
    total_tt_pruned: u64,
    total_incumbent_pruned: u64,
    total_por_pruned: u64,
) -> InCombatSolveResponse {
    let packed_state_bytes_total = selected.packed_state_bytes_total;
    let nodes_in_arena = selected.nodes_in_arena as u64;
    let avg_packed_bytes = if nodes_in_arena > 0 {
        packed_state_bytes_total / nodes_in_arena
    } else {
        0
    };

    let _ = selected.best_node;
    let _ = selected.root_baseline;

    InCombatSolveResponse {
        actions: selected.actions,
        score: selected.score,
        nodes_expanded: total_expanded,
        terminal_cards_hint: selected.terminal_cards_hint,
        stats: SearchStatistics {
            won: selected.won,
            strategic_hp_deficit: selected.strategic_hp_deficit,
            hp_lost: selected.hp_lost,
            potion_count: selected.potion_count,
            transition_count: total_transitions,
            choice_replay_attempts: total_choice_replays,
            tt_branches_pruned: total_tt_pruned,
            incumbent_branches_pruned: total_incumbent_pruned
                + incumbent_table.branches_pruned.load(Ordering::Relaxed) as u64,
            por_branches_pruned: total_por_pruned,
            avg_packed_state_bytes: avg_packed_bytes,
            elapsed_ms: started.elapsed().as_millis() as u64,
            closure_healing_certified: cert.healing_certified,
            closure_growth_zero_certified: cert.growth_certified_zero,
            termination: selected.termination.to_owned(),
        },
    }
}

pub struct CombatSearchCoordinator;

impl CombatSearchCoordinator {
    pub fn solve(
        root: &BattleState,
        policy: &SearchPolicy,
        content: &(dyn ContentDispatcher + Sync),
        cancel: &AtomicBool,
        progress_cb: Option<&(dyn Fn(&SearchProgress) + Sync)>,
    ) -> Result<InCombatSolveResponse, KernelError> {
        let compacted_opt = prepare_compacted_root(root, content);
        let root = compacted_opt.as_ref().unwrap_or(root);
        let started = Instant::now();
        let cert = ClosureCertificate::certify(root, content);
        let incumbent_table = PrimaryIncumbentTable::new();

        let dop = policy
            .max_degree_of_parallelism
            .clamp(1, crate::eval::weights::MAXIMUM_SEARCH_MAX_DEGREE_OF_PARALLELISM);
        let pool_ref = shared_search_pool(dop);
        let worker_count = if dop > 1 && pool_ref.is_some() {
            dop
        } else {
            1
        };
        let mut resources = PassResources::acquire(
            &root.static_ctx,
            worker_count,
            policy.transposition_entry_limit,
        );

        let base_profile = SolverSearchProfile {
            beam_width: policy.beam_width.max(1),
            potion_free_beam_width: policy.potion_free_beam_width,
            potion_beam_width: policy.potion_beam_width,
            max_expanded_nodes: policy.max_nodes.max(1),
            max_card_branches_per_node: policy.max_card_branches_per_node.max(1),
            max_pile_choice_branches_per_action: policy.max_pile_choice_branches_per_action.max(1),
            max_hand_choice_branches_per_action: policy.max_hand_choice_branches_per_action.max(1),
            soft_time_budget_ms: policy.time_budget_ms,
            reallocated_refinement_portfolio: policy.reallocated_refinement_portfolio,
            stop_portfolio_at_hp_target: policy.stop_portfolio_at_hp_target,
            ..Default::default()
        };

        let mut total_expanded = 0usize;
        let mut total_transitions = 0u64;
        let mut total_choice_replays = 0u64;
        let mut total_tt_pruned = 0u64;
        let mut total_incumbent_pruned = 0u64;
        let mut total_por_pruned = 0u64;

        let act_ending_boss = root.static_ctx.room_type == RoomType::Boss;
        let abort = SearchAbortSlot::new();

        let mut selected = if policy.use_novelty_search && !policy.adaptive_novelty_refinement {
            run_novelty_mode(
                root,
                content,
                policy,
                &base_profile,
                &cert,
                &incumbent_table,
                pool_ref,
                cancel,
                &abort,
                progress_cb,
                started,
                act_ending_boss,
                &mut total_expanded,
                &mut total_transitions,
                &mut total_choice_replays,
                &mut total_tt_pruned,
                &mut total_incumbent_pruned,
                &mut total_por_pruned,
                &mut resources,
            )?
        } else if policy.adaptive_novelty_refinement {
            run_adaptive_novelty_mode(
                root,
                content,
                policy,
                &base_profile,
                &cert,
                &incumbent_table,
                pool_ref,
                cancel,
                &abort,
                progress_cb,
                started,
                act_ending_boss,
                &mut total_expanded,
                &mut total_transitions,
                &mut total_choice_replays,
                &mut total_tt_pruned,
                &mut total_incumbent_pruned,
                &mut total_por_pruned,
                &mut resources,
            )?
        } else {
            solve_beam_or_portfolio(
                root,
                content,
                policy,
                &base_profile,
                &cert,
                &incumbent_table,
                pool_ref,
                cancel,
                &abort,
                progress_cb,
                &mut total_expanded,
                &mut total_transitions,
                &mut total_choice_replays,
                &mut total_tt_pruned,
                &mut total_incumbent_pruned,
                &mut total_por_pruned,
                &mut resources,
            )?
        };

        let etc_budget_ms = if policy.early_turn_exploration_budget_ms > 0 {
            policy.early_turn_exploration_budget_ms
        } else {
            policy.time_budget_ms
        };
        if policy.early_turn_exploration_depth > 0
            && !cancel.load(Ordering::Relaxed)
            && !has_reached_acceptable_battle_hp_loss(&selected.root_baseline, policy, &selected)
        {
            run_early_turn_exploration_pass(
                root,
                content,
                policy,
                &base_profile,
                &cert,
                &incumbent_table,
                pool_ref,
                cancel,
                &abort,
                started,
                etc_budget_ms,
                &mut total_expanded,
                &mut total_transitions,
                &mut total_choice_replays,
                &mut selected,
                &mut resources,
            )?;
        }

        if policy.use_boss_tempo_search
            && root.static_ctx.room_type == RoomType::Boss
            && !cancel.load(Ordering::Relaxed)
            && !can_finish_target_portfolio(
                &selected.root_baseline,
                policy,
                &base_profile,
                &selected,
                &cert,
            )
        {
            run_boss_tempo_post_pass(
                root,
                content,
                policy,
                &base_profile,
                &cert,
                &incumbent_table,
                pool_ref,
                cancel,
                &abort,
                &mut total_expanded,
                &mut total_transitions,
                &mut total_choice_replays,
                &mut selected,
                &mut resources,
            )?;
        }

        if let Some(err) = abort.clone_error() {
            return Err(err);
        }

        let is_cancelled = cancel.load(Ordering::Relaxed) || selected.termination == "Cancelled";
        if !is_cancelled
            && selected.root_baseline.has_active_hard_potion_constraint()
            && (selected.best_node.is_none() || !satisfies_run_output_hard_constraints(&selected))
        {
            return Err(KernelError::potion_policy_unsatisfied(
                if selected.root_baseline.all_forced_potion_mask != 0 {
                    "no explored route satisfied all forced potion directives".to_owned()
                } else {
                    format!(
                        "no explored route satisfied minimum required potion uses ({})",
                        selected.root_baseline.minimum_required_potion_uses
                    )
                },
            ));
        }

        Ok(build_solve_response(
            selected,
            cert,
            &incumbent_table,
            started,
            total_expanded,
            total_transitions,
            total_choice_replays,
            total_tt_pruned,
            total_incumbent_pruned,
            total_por_pruned,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_resolved(
        hash: u128,
        beam_score: f64,
        board: i32,
        opt_cards: i32,
        energy: i32,
        cum_hp_lost: i32,
        has_lookahead: bool,
        lookahead_victory: bool,
        lookahead_score: f64,
        lookahead_hp_lost: i32,
        lookahead_board_investment: i32,
    ) -> ResolvedChoiceCandidate {
        ResolvedChoiceCandidate {
            choices_offset: 0,
            choices_len: 1,
            depth0_sig: 0,
            action: CompactAction::new(CompactActionKind::EndTurn, 1, PorActionClass::Barrier).0,
            por_key: 0,
            has_extra_end_turn: false,
            state_slab_idx: 0,
            state_off: 0,
            state_len: 0,
            state_hash: StateHash(hash),
            turn: 1,
            turn_completed: false,
            action_count: 1,
            potion_count: 0,
            explicit_potion_count: 0,
            optional_paid_potion_count: 0,
            automatic_potion_count: 0,
            used_potion_slot_mask: 0,
            potion_strategic_cost: 0,
            optional_paid_potion_cost: 0,
            ambergris_count: 0,
            forced_used_mask: 0,
            forced_ambergris_count: 0,
            is_terminal: false,
            base_score: beam_score,
            fast_eval: StoredFastEval {
                energy: energy as i16,
                stars: 1,
                cumulative_hp_lost: cum_hp_lost as i16,
                ..Default::default()
            },
            full_eval: Some(StoredNodeEval {
                beam_rank_score: beam_score,
                board_investment: board,
                optionality_cards: opt_cards,
                ..Default::default()
            }),
            rare_eval: None,
            label: TranspositionLabel::new(0, 0, 0, 0, 0, cum_hp_lost, 1, beam_score),
            terminal_probe_score: 0.0,
            lookahead_score,
            lookahead_hp_lost,
            lookahead_board_investment,
            lookahead_setup_score: 0,
            has_lookahead,
            lookahead_victory,
        }
    }

    #[test]
    fn choice_sort_key_matches_resolved_choice_ordering() {
        let samples = [
            make_resolved(1, 100.0, 5, 2, 2, 4, true, false, 150.0, 4, 6),
            make_resolved(2, 100.0, 8, 1, 1, 6, true, false, 150.0, 4, 6),
            make_resolved(3, 120.0, 3, 4, 3, 2, true, true, 200.0, 1, 4),
            make_resolved(4, 90.0, 9, 5, 0, 1, false, false, 0.0, 1, 9),
            make_resolved(5, 110.0, 8, 3, 2, 4, true, false, 140.0, 3, 7),
        ];
        for criterion in 0..=3 {
            for a in &samples {
                let ka = ChoiceSortKey::from_candidate(a);
                for b in &samples {
                    let kb = ChoiceSortKey::from_candidate(b);
                    assert_eq!(
                        compare_choice_sort_key(&ka, &kb, criterion),
                        compare_resolved_choice(a, b, criterion),
                        "mismatch at criterion={criterion}"
                    );
                }
            }
        }
    }
}
