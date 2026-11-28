use crate::arena::{MainStateArena, NodeArena, NodeId};
use crate::coordinator::clone_battle_state_into;
use crate::eval::{
    allows_brightest_flame, compute_beam_rank_score_with_centi, evaluate_fast,
    evaluate_fast_and_full_fused, is_creature_reviving, ActEndingBossPolicy, BeamWeightTerm,
    EvalMode, FastEvalSummary, FullEvalDetails, IntentHitsCache, PotionUsePolicy,
    RelicCounterId, RelicCounterPolicy, RootEvalBaseline, SolverPotionPolicy,
};
use crate::hash::StateHash;
use crate::packer::{empty_scratch_state, hash_canonical_key, unpack_state};
use crate::portfolio::SolverSearchProfile;
use crate::SearchPolicy;
use smallvec::SmallVec;
use std::sync::atomic::{fence, AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;
use sts2_kernel::ordering::{
    card_has_dynamic_comparison_values, resolve_energy_cost, resolve_local_energy_cost_card,
    resolve_local_star_cost_card,
};
use sts2_kernel::content_dispatch::{
    CachedHookListener, CardRolloutProfile, GenericChoiceQualityEntry,
};
use sts2_kernel::state::{
    hot_keys, BattleState, CardHandle, CardTempFlags, CreatureHandle, FieldValue,
    RootStaticContext,
};
use sts2_kernel::{
    rollout_meta_flags as mf, rollout_op_flags as op, CombatEngine, ContentDispatcher, StepOutcome,
};
use sts2_types::{
    AfflictionId, CardId, CardKeywords, CardRarity, CardStaticFlags, CardTags, CardType, Dec,
    EnchantmentId, HookSlot, MonsterId, OrbId, PileType, PlayerTurnPhase, PowerId, PowerType,
    RelicId, TargetType,
};

const MAX_ROLLOUT_ACTIONS_PER_TURN: usize = 24;
const MAX_TOTAL_ROLLOUT_STEPS: usize = 96;
const MAX_FAST_ENEMIES: usize = 16;

const ATTACK_RELIC_MASK: u64 =
    (1u64 << (RelicCounterId::PenNib as u32)) | (1u64 << (RelicCounterId::Nunchaku as u32));
const SKILL_RELIC_MASK: u64 = 1u64 << (RelicCounterId::TuningFork as u32);
const CARD_RELIC_MASK: u64 = 1u64 << (RelicCounterId::IronClub as u32);
const EXHAUST_RELIC_MASK: u64 = 1u64 << (RelicCounterId::JossPaper as u32);
const STAR_RELIC_MASK: u64 = 1u64 << (RelicCounterId::GalacticDust as u32);
const TURN_TICK_RELIC_MASK: u64 = (1u64 << (RelicCounterId::HappyFlower as u32))
    | (1u64 << (RelicCounterId::FakeHappyFlower as u32))
    | (1u64 << (RelicCounterId::Pendulum as u32))
    | (1u64 << (RelicCounterId::PollinousCore as u32));

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PilotKind {
    DefensiveTempo,
    SetupFirst,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CompactRolloutStep {
    pub card_handle: u32,
    pub target_handle: u32,
    pub choices_offset: u32,
    pub choices_len: u16,
    pub is_end_turn: u8,
    pub _pad: u8,
}

const _: () = assert!(std::mem::size_of::<CompactRolloutStep>() == 16);

#[derive(Clone, Debug, Default)]
pub struct RolloutChoiceSlab {
    pub steps_meta: SmallVec<[(u32, u16); 4]>,
    pub indices: SmallVec<[u16; 8]>,
}

impl RolloutChoiceSlab {
    #[inline]
    pub fn clear(&mut self) {
        self.steps_meta.clear();
        self.indices.clear();
    }

    #[inline]
    pub fn truncate_to(&mut self, meta_len: usize, indices_len: usize) {
        self.steps_meta.truncate(meta_len);
        self.indices.truncate(indices_len);
    }

    #[inline]
    pub fn push_step(&mut self, sel: &[usize]) {
        let idx_off = self.indices.len() as u32;
        for &v in sel {
            self.indices.push(v as u16);
        }
        self.steps_meta.push((idx_off, sel.len() as u16));
    }

    #[inline]
    pub fn step_indices(&self, step_idx: usize) -> &[u16] {
        let (off, len) = self.steps_meta[step_idx];
        let o = off as usize;
        &self.indices[o..o + len as usize]
    }
}

#[derive(Clone, Debug)]
pub struct PromotedVictory {
    pub steps: SmallVec<[CompactRolloutStep; 32]>,
    pub choices: RolloutChoiceSlab,
    pub final_fast: FastEvalSummary,
    pub final_turn: i32,
    pub final_state_hash: StateHash,
    pub auto_potions_used: u16,
}

#[derive(Clone, Debug)]
pub struct RolloutResult {
    pub rollout_score: f64,
    pub promoted_victory: Option<PromotedVictory>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RolloutDedupKey(u128);

impl RolloutDedupKey {
    #[inline(always)]
    fn new(
        card_id: CardId,
        upgrade_level: i32,
        energy_cost: i32,
        star_cost: i32,
        has_ext: bool,
        aff_id: Option<sts2_types::AfflictionId>,
    ) -> Self {
        let aff_bits = aff_id.map_or(0u128, |a| {
            debug_assert!((a as u32) < 0x7FFF);
            u128::from((a as u32) + 1)
        });
        let raw = u128::from(card_id as u16)
            | (u128::from(has_ext) << 16)
            | (aff_bits << 17)
            | (u128::from(upgrade_level as u32) << 32)
            | (u128::from(energy_cost as u32) << 64)
            | (u128::from(star_cost as u32) << 96);
        Self(raw)
    }

    #[inline(always)]
    const fn try_fast_u64(
        card_id: CardId,
        upgrade_level: i32,
        energy_cost: i32,
        star_cost: i32,
        has_ext: bool,
        aff_id: Option<sts2_types::AfflictionId>,
    ) -> Option<u64> {
        let cid = card_id as u32;
        let upg = upgrade_level as u32;
        let e = (energy_cost.wrapping_add(1)) as u32;
        let s = (star_cost.wrapping_add(1)) as u32;
        let aff_ok = match aff_id {
            None => true,
            Some(a) => (a as u32) < 63,
        };
        if cid < 2048 && upg < 65536 && e < 32768 && s < 32768 && aff_ok {
            let aff_bits = match aff_id {
                None => 0u64,
                Some(a) => (a as u64) + 1,
            };
            Some(
                (cid as u64)
                    | ((has_ext as u64) << 11)
                    | (aff_bits << 12)
                    | ((upg as u64) << 18)
                    | ((e as u64) << 34)
                    | ((s as u64) << 49),
            )
        } else {
            None
        }
    }

    #[inline(always)]
    fn from_fast_u64(raw: u64) -> Self {
        let cid = raw & 0x7FF;
        let has_ext = (raw >> 11) & 1;
        let aff_bits = (raw >> 12) & 0x3F;
        let upg = (raw >> 18) & 0xFFFF;
        let e = (((raw >> 34) & 0x7FFF) as u32).wrapping_sub(1);
        let s = (((raw >> 49) & 0x7FFF) as u32).wrapping_sub(1);
        Self(
            u128::from(cid)
                | (u128::from(has_ext) << 16)
                | (u128::from(aff_bits) << 17)
                | (u128::from(upg) << 32)
                | (u128::from(e) << 64)
                | (u128::from(s) << 96),
        )
    }
}

#[inline(always)]
const fn eval_mode_index(mode: EvalMode) -> usize {
    match mode {
        EvalMode::Balanced => 0,
        EvalMode::SetupAndSurvival => 1,
        EvalMode::Offense => 2,
        EvalMode::DesperateRescue => 3,
    }
}

#[inline(always)]
const fn eval_mode_potion_weight(mode: EvalMode) -> f64 {
    match mode {
        EvalMode::Balanced => 390.0,
        EvalMode::SetupAndSurvival => 420.0,
        EvalMode::Offense => 300.0,
        EvalMode::DesperateRescue => 240.0,
    }
}

#[inline(always)]
fn compute_scores_at_zero(
    fast: &FastEvalSummary,
    full: &FullEvalDetails,
    root_baseline: &RootEvalBaseline,
    profile: &SolverSearchProfile,
    linear_adj: bool,
    action_count: u16,
) -> [f64; 4] {
    if !linear_adj || fast.won {
        let s = compute_beam_rank_score_with_centi(
            fast,
            full,
            root_baseline,
            profile.base_score_only,
            profile.boss_tempo_hp_pricing,
            profile.beam_weight_perturbation,
            EvalMode::Balanced,
            0,
            action_count,
        );
        [s, s, s, s]
    } else {
        [
            compute_beam_rank_score_with_centi(
                fast,
                full,
                root_baseline,
                profile.base_score_only,
                profile.boss_tempo_hp_pricing,
                profile.beam_weight_perturbation,
                EvalMode::Balanced,
                0,
                action_count,
            ),
            compute_beam_rank_score_with_centi(
                fast,
                full,
                root_baseline,
                profile.base_score_only,
                profile.boss_tempo_hp_pricing,
                profile.beam_weight_perturbation,
                EvalMode::SetupAndSurvival,
                0,
                action_count,
            ),
            compute_beam_rank_score_with_centi(
                fast,
                full,
                root_baseline,
                profile.base_score_only,
                profile.boss_tempo_hp_pricing,
                profile.beam_weight_perturbation,
                EvalMode::Offense,
                0,
                action_count,
            ),
            compute_beam_rank_score_with_centi(
                fast,
                full,
                root_baseline,
                profile.base_score_only,
                profile.boss_tempo_hp_pricing,
                profile.beam_weight_perturbation,
                EvalMode::DesperateRescue,
                0,
                action_count,
            ),
        ]
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ChoiceProbeResult {
    pub score: f64,
    pub hp_lost: i32,
    pub setup_score: i32,
    pub board_investment: i32,
    pub strategy_goal_hp_credit: i32,
    #[allow(dead_code)]
    pub strategic_hp_credit: i32,
    pub is_victory: bool,
    pub is_defeat: bool,
    pub is_event_defeat: bool,
}

const CHOICE_PROBE_SHARDS: usize = 256;
const CHOICE_PROBE_SETS_PER_SHARD: usize = 128;
const CHOICE_PROBE_WAYS: usize = 4;
const CHOICE_PROBE_SLOTS_PER_SHARD: usize = CHOICE_PROBE_SETS_PER_SHARD * CHOICE_PROBE_WAYS;
const CHOICE_PROBE_L1_SETS: usize = 512;
const CHOICE_PROBE_L1_WAYS: usize = 2;
const CHOICE_PROBE_L1_SLOTS: usize = CHOICE_PROBE_L1_SETS * CHOICE_PROBE_L1_WAYS;

#[inline(always)]
fn choice_probe_slot_tag(exact_state_hash: StateHash, cfg_hash: u64, epoch: u32) -> u64 {
    let fp32 = ((exact_state_hash.0 >> 64) as u32)
        ^ (cfg_hash as u32)
        ^ ((cfg_hash >> 32) as u32);
    (fp32 as u64) | ((epoch as u64) << 32)
}
const CHOICE_PROBE_FLAG_SOME: u8 = 1 << 0;
const CHOICE_PROBE_FLAG_NONE: u8 = 1 << 1;
const CHOICE_PROBE_FLAG_VICTORY: u8 = 1 << 2;
const CHOICE_PROBE_FLAG_DEFEAT: u8 = 1 << 3;
const CHOICE_PROBE_FLAG_LINEAR_ADJ: u8 = 1 << 4;
const CHOICE_PROBE_FLAG_EVENT_DEFEAT: u8 = 1 << 5;

#[inline]
fn mix_cfg_u64(acc: u64, v: u64) -> u64 {
    let mut x = acc.wrapping_add(v).wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

#[inline]
fn choice_probe_cfg_hash(profile: &SolverSearchProfile, max_turns_ahead: u8) -> u64 {
    let flags_word = ((max_turns_ahead as u64) << 4)
        | (u64::from(profile.base_score_only) << 12);
    let (pert_term, pert_scale) = match profile.beam_weight_perturbation {
        None => (0u64, 0u64),
        Some(p) => {
            let term = match p.term {
                BeamWeightTerm::CurrentEnergy => 1u64,
                BeamWeightTerm::PersistentBuffDelta => 2u64,
                BeamWeightTerm::EnemyHp => 3u64,
            };
            (term, p.scale.to_bits())
        }
    };
    let h = mix_cfg_u64(0x517C_C1B7_2722_0A95, flags_word | (pert_term << 16));
    mix_cfg_u64(h, pert_scale)
}

#[derive(Clone, Copy)]
pub(crate) struct CachedProbeEntry {
    pub scores_at_zero: [f64; 4],
    pub hp_lost: i32,
    pub setup_score: i32,
    pub board_investment: i32,
    pub strategy_goal_hp_credit: i32,
    pub strategic_hp_credit: i32,
    pub alchemize_count: u8,
    pub empty_potion_slots: u8,
    pub is_victory: bool,
    pub is_defeat: bool,
    pub is_event_defeat: bool,
    pub linear_adj: bool,
}

impl CachedProbeEntry {
    #[inline(always)]
    pub(crate) fn materialize_result(
        self,
        root_baseline: &RootEvalBaseline,
        eval_mode: EvalMode,
        optional_paid_potion_cost: i32,
        optional_paid_potion_count: usize,
        parent_action_count: u16,
    ) -> ChoiceProbeResult {
        let base = self.scores_at_zero[eval_mode_index(eval_mode)];
        let score = if self.linear_adj {
            let net_potion_cost_centi = PotionUsePolicy::compute_net_potion_opportunity_cost_centi(
                optional_paid_potion_cost,
                optional_paid_potion_count as i32,
                self.alchemize_count as i32,
                self.empty_potion_slots as i32,
                root_baseline,
            );
            let act_coeff = if self.is_victory { 10.0 } else { 5.0 };
            let pot_coeff = if self.is_victory {
                390.0
            } else {
                eval_mode_potion_weight(eval_mode)
            };
            base - (net_potion_cost_centi as f64) * pot_coeff - (parent_action_count as f64) * act_coeff
        } else {
            base
        };
        ChoiceProbeResult {
            score,
            hp_lost: self.hp_lost,
            setup_score: self.setup_score,
            board_investment: self.board_investment,
            strategy_goal_hp_credit: self.strategy_goal_hp_credit,
            strategic_hp_credit: self.strategic_hp_credit,
            is_victory: self.is_victory,
            is_defeat: self.is_defeat,
            is_event_defeat: self.is_event_defeat,
        }
    }
}

#[derive(Clone, Copy)]
struct ChoiceProbeSlot {
    state_hash: StateHash,
    cfg_hash: u64,
    scores_at_zero: [f64; 4],
    hp_lost: i32,
    setup_score: i32,
    board_investment: i32,
    strategy_goal_hp_credit: i32,
    strategic_hp_credit: i32,
    epoch: u32,
    flags: u8,
    alchemize_count: u8,
    empty_potion_slots: u8,
}

const _: () = assert!(std::mem::size_of::<ChoiceProbeSlot>() <= 96);

impl ChoiceProbeSlot {
    const EMPTY: Self = Self {
        state_hash: StateHash(0),
        cfg_hash: 0,
        scores_at_zero: [0.0; 4],
        hp_lost: 0,
        setup_score: 0,
        board_investment: 0,
        strategy_goal_hp_credit: 0,
        strategic_hp_credit: 0,
        epoch: 0,
        flags: 0,
        alchemize_count: 0,
        empty_potion_slots: 0,
    };

    #[inline(always)]
    fn into_entry(self) -> CachedProbeEntry {
        CachedProbeEntry {
            scores_at_zero: self.scores_at_zero,
            hp_lost: self.hp_lost,
            setup_score: self.setup_score,
            board_investment: self.board_investment,
            strategy_goal_hp_credit: self.strategy_goal_hp_credit,
            strategic_hp_credit: self.strategic_hp_credit,
            alchemize_count: self.alchemize_count,
            empty_potion_slots: self.empty_potion_slots,
            is_victory: (self.flags & CHOICE_PROBE_FLAG_VICTORY) != 0,
            is_defeat: (self.flags & CHOICE_PROBE_FLAG_DEFEAT) != 0,
            is_event_defeat: (self.flags & CHOICE_PROBE_FLAG_EVENT_DEFEAT) != 0,
            linear_adj: (self.flags & CHOICE_PROBE_FLAG_LINEAR_ADJ) != 0,
        }
    }
}

#[repr(C, align(64))]
struct ChoiceProbeAtomicSlot {
    seq: AtomicU32,
    _pad: u32,
    words: [AtomicU64; 11],
}

impl ChoiceProbeAtomicSlot {
    #[inline]
    const fn new() -> Self {
        Self {
            seq: AtomicU32::new(0),
            _pad: 0,
            words: [const { AtomicU64::new(0) }; 11],
        }
    }
}

struct ChoiceProbeShardTable {
    tags: Box<[[AtomicU64; CHOICE_PROBE_WAYS]; CHOICE_PROBE_SETS_PER_SHARD]>,
    slots: Box<[ChoiceProbeAtomicSlot; CHOICE_PROBE_SLOTS_PER_SHARD]>,
    victim_cursor: Box<[AtomicU8; CHOICE_PROBE_SETS_PER_SHARD]>,
}

impl ChoiceProbeShardTable {
    #[cold]
    #[inline(never)]
    fn alloc() -> Self {
        let tags: Box<[[AtomicU64; CHOICE_PROBE_WAYS]; CHOICE_PROBE_SETS_PER_SHARD]> =
            (0..CHOICE_PROBE_SETS_PER_SHARD)
                .map(|_| std::array::from_fn(|_| AtomicU64::new(0)))
                .collect::<Vec<_>>()
                .into_boxed_slice()
                .try_into()
                .unwrap_or_else(|_| unreachable!());
        let slots: Box<[ChoiceProbeAtomicSlot; CHOICE_PROBE_SLOTS_PER_SHARD]> =
            (0..CHOICE_PROBE_SLOTS_PER_SHARD)
                .map(|_| ChoiceProbeAtomicSlot::new())
                .collect::<Vec<_>>()
                .into_boxed_slice()
                .try_into()
                .unwrap_or_else(|_| unreachable!());
        let victim_cursor: Box<[AtomicU8; CHOICE_PROBE_SETS_PER_SHARD]> =
            (0..CHOICE_PROBE_SETS_PER_SHARD)
                .map(|_| AtomicU8::new(0))
                .collect::<Vec<_>>()
                .into_boxed_slice()
                .try_into()
                .unwrap_or_else(|_| unreachable!());
        Self {
            tags,
            slots,
            victim_cursor,
        }
    }
}

#[derive(Default)]
struct ChoiceProbeShard {
    table: OnceLock<ChoiceProbeShardTable>,
}

pub(crate) struct ChoiceProbeCache {
    shards: Box<[ChoiceProbeShard]>,
    epoch: AtomicU32,
}

impl Default for ChoiceProbeCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ChoiceProbeCache {
    pub(crate) fn new() -> Self {
        let shards = (0..CHOICE_PROBE_SHARDS)
            .map(|_| ChoiceProbeShard::default())
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            shards,
            epoch: AtomicU32::new(1),
        }
    }

    pub(crate) fn clear(&self) {
        let mut next = self.epoch.load(Ordering::Relaxed).wrapping_add(1);
        if next == 0 {
            next = 1;
            for shard in self.shards.iter() {
                if let Some(tbl) = shard.table.get() {
                    for row in tbl.tags.iter() {
                        for t in row {
                            t.store(0, Ordering::Relaxed);
                        }
                    }
                    for slot in tbl.slots.iter() {
                        let seq = slot.seq.load(Ordering::Acquire);
                        if (seq & 1) == 0 && seq != 0 {
                            if slot
                                .seq
                                .compare_exchange(
                                    seq,
                                    seq.wrapping_add(1),
                                    Ordering::Acquire,
                                    Ordering::Relaxed,
                                )
                                .is_ok()
                            {
                                fence(Ordering::Release);
                                slot.words[10].store(0, Ordering::Relaxed);
                                let ns = seq.wrapping_add(2);
                                slot.seq
                                    .store(if ns == 0 { 2 } else { ns }, Ordering::Release);
                            }
                        }
                    }
                    for c in tbl.victim_cursor.iter() {
                        c.store(0, Ordering::Relaxed);
                    }
                }
            }
        }
        self.epoch.store(next, Ordering::Release);
    }

    #[inline(always)]
    fn slot_indices(exact_state_hash: StateHash, cfg_hash: u64) -> (usize, usize) {
        let shard_idx = exact_state_hash.shard_index() & (CHOICE_PROBE_SHARDS - 1);
        let lo = exact_state_hash.0 as usize;
        let set_idx =
            (lo ^ (lo >> 16) ^ (cfg_hash as usize)) & (CHOICE_PROBE_SETS_PER_SHARD - 1);
        (shard_idx, set_idx)
    }

    #[inline(always)]
    fn try_read_slot(
        slot: &ChoiceProbeAtomicSlot,
        lo: u64,
        hi: u64,
        cfg_hash: u64,
        epoch: u32,
    ) -> Option<(u32, Option<CachedProbeEntry>)> {
        let seq1 = slot.seq.load(Ordering::Acquire);
        if (seq1 & 1) != 0 || seq1 == 0 {
            return None;
        }
        let w0 = slot.words[0].load(Ordering::Relaxed);
        let w1 = slot.words[1].load(Ordering::Relaxed);
        let w2 = slot.words[2].load(Ordering::Relaxed);
        let w3 = slot.words[3].load(Ordering::Relaxed);
        let w4 = slot.words[4].load(Ordering::Relaxed);
        let w5 = slot.words[5].load(Ordering::Relaxed);
        let w6 = slot.words[6].load(Ordering::Relaxed);
        let w7 = slot.words[7].load(Ordering::Relaxed);
        let w8 = slot.words[8].load(Ordering::Relaxed);
        let w9 = slot.words[9].load(Ordering::Relaxed);
        let w10 = slot.words[10].load(Ordering::Relaxed);
        fence(Ordering::Acquire);
        let seq2 = slot.seq.load(Ordering::Relaxed);
        if seq1 != seq2 {
            return None;
        }
        let slot_epoch = w10 as u32;
        let flags = (w10 >> 32) as u8;
        if w0 == lo && w1 == hi && w2 == cfg_hash && slot_epoch == epoch && flags != 0 {
            if (flags & CHOICE_PROBE_FLAG_NONE) != 0 {
                return Some((seq1, None));
            }
            return Some((
                seq1,
                Some(CachedProbeEntry {
                    scores_at_zero: [
                        f64::from_bits(w3),
                        f64::from_bits(w4),
                        f64::from_bits(w5),
                        f64::from_bits(w6),
                    ],
                    hp_lost: w7 as u32 as i32,
                    board_investment: (w7 >> 32) as u32 as i32,
                    setup_score: w8 as u32 as i32,
                    strategy_goal_hp_credit: (w8 >> 32) as u32 as i32,
                    strategic_hp_credit: w9 as u32 as i32,
                    alchemize_count: (w10 >> 40) as u8,
                    empty_potion_slots: (w10 >> 48) as u8,
                    is_victory: (flags & CHOICE_PROBE_FLAG_VICTORY) != 0,
                    is_defeat: (flags & CHOICE_PROBE_FLAG_DEFEAT) != 0,
                    is_event_defeat: (flags & CHOICE_PROBE_FLAG_EVENT_DEFEAT) != 0,
                    linear_adj: (flags & CHOICE_PROBE_FLAG_LINEAR_ADJ) != 0,
                }),
            ));
        }
        None
    }

    #[inline(always)]
    fn table_get(
        tbl: &ChoiceProbeShardTable,
        set_idx: usize,
        want_tag: u64,
        exact_state_hash: StateHash,
        cfg_hash: u64,
        epoch: u32,
    ) -> Option<Option<CachedProbeEntry>> {
        let lo = exact_state_hash.0 as u64;
        let hi = (exact_state_hash.0 >> 64) as u64;
        let base = set_idx * CHOICE_PROBE_WAYS;
        let tag_row = &tbl.tags[set_idx];
        for w in 0..CHOICE_PROBE_WAYS {
            if tag_row[w].load(Ordering::Acquire) == want_tag {
                if let Some((_seq, entry)) =
                    Self::try_read_slot(&tbl.slots[base + w], lo, hi, cfg_hash, epoch)
                {
                    return Some(entry);
                }
            }
        }
        None
    }

    #[inline(always)]
    fn table_insert(
        tbl: &ChoiceProbeShardTable,
        set_idx: usize,
        want_tag: u64,
        exact_state_hash: StateHash,
        cfg_hash: u64,
        epoch: u32,
        slot: &ChoiceProbeSlot,
    ) {
        debug_assert_ne!(slot.flags, 0);
        let lo = exact_state_hash.0 as u64;
        let hi = (exact_state_hash.0 >> 64) as u64;
        let base = set_idx * CHOICE_PROBE_WAYS;
        let tag_row = &tbl.tags[set_idx];

        let mut target: Option<(usize, u32)> = None;
        let mut empty_way: Option<(usize, u32)> = None;
        for w in 0..CHOICE_PROBE_WAYS {
            let t = tag_row[w].load(Ordering::Acquire);
            let s = &tbl.slots[base + w];
            if t == want_tag {
                if let Some((seq, _)) = Self::try_read_slot(s, lo, hi, cfg_hash, epoch) {
                    target = Some((w, seq));
                    break;
                }
            } else if empty_way.is_none() && (t >> 32) as u32 != epoch {
                let seq = s.seq.load(Ordering::Acquire);
                if (seq & 1) == 0 {
                    empty_way = Some((w, seq));
                }
            }
        }

        let (way, expected_seq) = if let Some(hit) = target {
            hit
        } else if let Some(ew) = empty_way {
            ew
        } else {
            let w = (tbl.victim_cursor[set_idx].fetch_add(1, Ordering::Relaxed) as usize)
                & (CHOICE_PROBE_WAYS - 1);
            let seq = tbl.slots[base + w].seq.load(Ordering::Acquire);
            if (seq & 1) != 0 {
                return;
            }
            (w, seq)
        };

        let dst = &tbl.slots[base + way];
        if dst
            .seq
            .compare_exchange(
                expected_seq,
                expected_seq.wrapping_add(1),
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .is_err()
        {
            return;
        }
        fence(Ordering::Release);
        dst.words[0].store(lo, Ordering::Relaxed);
        dst.words[1].store(hi, Ordering::Relaxed);
        dst.words[2].store(cfg_hash, Ordering::Relaxed);
        dst.words[3].store(slot.scores_at_zero[0].to_bits(), Ordering::Relaxed);
        dst.words[4].store(slot.scores_at_zero[1].to_bits(), Ordering::Relaxed);
        dst.words[5].store(slot.scores_at_zero[2].to_bits(), Ordering::Relaxed);
        dst.words[6].store(slot.scores_at_zero[3].to_bits(), Ordering::Relaxed);
        dst.words[7].store(
            ((slot.hp_lost as u32) as u64) | (((slot.board_investment as u32) as u64) << 32),
            Ordering::Relaxed,
        );
        dst.words[8].store(
            ((slot.setup_score as u32) as u64)
                | (((slot.strategy_goal_hp_credit as u32) as u64) << 32),
            Ordering::Relaxed,
        );
        dst.words[9].store((slot.strategic_hp_credit as u32) as u64, Ordering::Relaxed);
        dst.words[10].store(
            (epoch as u64)
                | (u64::from(slot.flags) << 32)
                | (u64::from(slot.alchemize_count) << 40)
                | (u64::from(slot.empty_potion_slots) << 48),
            Ordering::Relaxed,
        );
        let ns = expected_seq.wrapping_add(2);
        dst.seq
            .store(if ns == 0 { 2 } else { ns }, Ordering::Release);
        tag_row[way].store(want_tag, Ordering::Release);
    }

    #[inline]
    pub(crate) fn get(
        &self,
        exact_state_hash: StateHash,
        cfg_hash: u64,
    ) -> Option<Option<CachedProbeEntry>> {
        let epoch = self.epoch.load(Ordering::Acquire);
        let (shard_idx, set_idx) = Self::slot_indices(exact_state_hash, cfg_hash);
        let want_tag = choice_probe_slot_tag(exact_state_hash, cfg_hash, epoch);
        let tbl = self.shards[shard_idx].table.get()?;
        Self::table_get(tbl, set_idx, want_tag, exact_state_hash, cfg_hash, epoch)
    }

    #[inline]
    pub(crate) fn insert(
        &self,
        exact_state_hash: StateHash,
        cfg_hash: u64,
        res: Option<CachedProbeEntry>,
    ) {
        let epoch = self.epoch.load(Ordering::Acquire);
        let (shard_idx, set_idx) = Self::slot_indices(exact_state_hash, cfg_hash);
        let want_tag = choice_probe_slot_tag(exact_state_hash, cfg_hash, epoch);
        let slot = Self::build_slot(exact_state_hash, cfg_hash, epoch, res);
        let tbl = self.shards[shard_idx]
            .table
            .get_or_init(ChoiceProbeShardTable::alloc);
        Self::table_insert(tbl, set_idx, want_tag, exact_state_hash, cfg_hash, epoch, &slot);
    }

    #[inline]
    pub(crate) fn get_single_thread(
        &mut self,
        exact_state_hash: StateHash,
        cfg_hash: u64,
    ) -> Option<Option<CachedProbeEntry>> {
        self.get(exact_state_hash, cfg_hash)
    }

    #[inline]
    pub(crate) fn insert_single_thread(
        &mut self,
        exact_state_hash: StateHash,
        cfg_hash: u64,
        res: Option<CachedProbeEntry>,
    ) {
        self.insert(exact_state_hash, cfg_hash, res);
    }

    #[inline(always)]
    fn build_slot(
        exact_state_hash: StateHash,
        cfg_hash: u64,
        epoch: u32,
        res: Option<CachedProbeEntry>,
    ) -> ChoiceProbeSlot {
        match res {
            None => ChoiceProbeSlot {
                state_hash: exact_state_hash,
                cfg_hash,
                scores_at_zero: [0.0; 4],
                hp_lost: 0,
                setup_score: 0,
                board_investment: 0,
                strategy_goal_hp_credit: 0,
                strategic_hp_credit: 0,
                epoch,
                flags: CHOICE_PROBE_FLAG_NONE,
                alchemize_count: 0,
                empty_potion_slots: 0,
            },
            Some(r) => {
                let mut flags = CHOICE_PROBE_FLAG_SOME;
                if r.is_victory {
                    flags |= CHOICE_PROBE_FLAG_VICTORY;
                }
                if r.is_defeat {
                    flags |= CHOICE_PROBE_FLAG_DEFEAT;
                }
                if r.is_event_defeat {
                    flags |= CHOICE_PROBE_FLAG_EVENT_DEFEAT;
                }
                if r.linear_adj {
                    flags |= CHOICE_PROBE_FLAG_LINEAR_ADJ;
                }
                ChoiceProbeSlot {
                    state_hash: exact_state_hash,
                    cfg_hash,
                    scores_at_zero: r.scores_at_zero,
                    hp_lost: r.hp_lost,
                    setup_score: r.setup_score,
                    board_investment: r.board_investment,
                    strategy_goal_hp_credit: r.strategy_goal_hp_credit,
                    strategic_hp_credit: r.strategic_hp_credit,
                    epoch,
                    flags,
                    alchemize_count: r.alchemize_count,
                    empty_potion_slots: r.empty_potion_slots,
                }
            }
        }
    }

    #[inline(always)]
    fn l1_set_index(exact_state_hash: StateHash, cfg_hash: u64) -> usize {
        let lo = exact_state_hash.0 as usize;
        (lo ^ (lo >> 16) ^ (cfg_hash as usize)) & (CHOICE_PROBE_L1_SETS - 1)
    }

    #[inline(always)]
    fn write_l1(
        l1: &mut ChoiceProbeL1,
        epoch: u32,
        exact_state_hash: StateHash,
        cfg_hash: u64,
        res: Option<CachedProbeEntry>,
    ) {
        let tbl = l1.table.get_or_insert_with(ChoiceProbeL1Table::alloc);
        if l1.epoch != epoch {
            if epoch < l1.epoch {
                tbl.tags.fill([0u64; CHOICE_PROBE_L1_WAYS]);
                for slot in tbl.slots.iter_mut() {
                    slot.flags = 0;
                    slot.epoch = 0;
                }
                tbl.cursor.fill(0);
            }
            l1.epoch = epoch;
        }
        let set_idx = Self::l1_set_index(exact_state_hash, cfg_hash);
        let want_tag = choice_probe_slot_tag(exact_state_hash, cfg_hash, epoch);
        let base = set_idx * CHOICE_PROBE_L1_WAYS;
        let slot = Self::build_slot(exact_state_hash, cfg_hash, epoch, res);
        debug_assert_ne!(slot.flags, 0);
        let tag_row = &mut tbl.tags[set_idx];
        let l1_slots = &mut tbl.slots;
        let mut empty_way: Option<usize> = None;
        for w in 0..CHOICE_PROBE_L1_WAYS {
            let t = tag_row[w];
            if t == want_tag {
                let cur = &l1_slots[base + w];
                if cur.epoch == epoch
                    && cur.flags != 0
                    && cur.cfg_hash == cfg_hash
                    && cur.state_hash == exact_state_hash
                {
                    l1_slots[base + w] = slot;
                    return;
                }
            } else if empty_way.is_none() && (t >> 32) as u32 != epoch {
                empty_way = Some(w);
            }
        }
        let way = if let Some(w) = empty_way {
            w
        } else {
            let w = usize::from(tbl.cursor[set_idx]) & (CHOICE_PROBE_L1_WAYS - 1);
            tbl.cursor[set_idx] = tbl.cursor[set_idx].wrapping_add(1);
            w
        };
        tag_row[way] = want_tag;
        l1_slots[base + way] = slot;
    }

    #[inline]
    pub(crate) fn get_with_l1(
        &self,
        l1: &mut ChoiceProbeL1,
        exact_state_hash: StateHash,
        cfg_hash: u64,
    ) -> Option<Option<CachedProbeEntry>> {
        if !l1.use_l1 {
            return self.get(exact_state_hash, cfg_hash);
        }
        let epoch = self.epoch.load(Ordering::Acquire);
        if l1.epoch == epoch {
            if let Some(tbl) = l1.table.as_ref() {
                let set_idx = Self::l1_set_index(exact_state_hash, cfg_hash);
                let want_tag = choice_probe_slot_tag(exact_state_hash, cfg_hash, epoch);
                let tag_row = tbl.tags[set_idx];
                let base = set_idx * CHOICE_PROBE_L1_WAYS;
                for w in 0..CHOICE_PROBE_L1_WAYS {
                    if tag_row[w] == want_tag {
                        let s = &tbl.slots[base + w];
                        if s.state_hash == exact_state_hash
                            && s.epoch == epoch
                            && s.flags != 0
                            && s.cfg_hash == cfg_hash
                        {
                            if (s.flags & CHOICE_PROBE_FLAG_NONE) != 0 {
                                return Some(None);
                            }
                            return Some(Some(s.into_entry()));
                        }
                    }
                }
            }
        }
        let res = self.get(exact_state_hash, cfg_hash)?;
        Self::write_l1(l1, epoch, exact_state_hash, cfg_hash, res);
        Some(res)
    }

    #[inline]
    pub(crate) fn insert_with_l1(
        &self,
        l1: &mut ChoiceProbeL1,
        exact_state_hash: StateHash,
        cfg_hash: u64,
        res: Option<CachedProbeEntry>,
    ) {
        if !l1.use_l1 {
            self.insert(exact_state_hash, cfg_hash, res);
            return;
        }
        let epoch = self.epoch.load(Ordering::Acquire);
        Self::write_l1(l1, epoch, exact_state_hash, cfg_hash, res);
        self.insert(exact_state_hash, cfg_hash, res);
    }
}

pub(crate) enum ProbeCacheAccess<'a> {
    SingleThread(&'a mut ChoiceProbeCache),
    Shared(&'a ChoiceProbeCache),
}

impl<'a> ProbeCacheAccess<'a> {
    #[inline(always)]
    pub(crate) fn get_with_l1(
        &mut self,
        l1: &mut ChoiceProbeL1,
        exact_state_hash: StateHash,
        cfg_hash: u64,
    ) -> Option<Option<CachedProbeEntry>> {
        match self {
            Self::SingleThread(cache) => cache.get_single_thread(exact_state_hash, cfg_hash),
            Self::Shared(cache) => cache.get_with_l1(l1, exact_state_hash, cfg_hash),
        }
    }

    #[inline(always)]
    pub(crate) fn insert_with_l1(
        &mut self,
        l1: &mut ChoiceProbeL1,
        exact_state_hash: StateHash,
        cfg_hash: u64,
        res: Option<CachedProbeEntry>,
    ) {
        match self {
            Self::SingleThread(cache) => {
                cache.insert_single_thread(exact_state_hash, cfg_hash, res);
            }
            Self::Shared(cache) => {
                cache.insert_with_l1(l1, exact_state_hash, cfg_hash, res);
            }
        }
    }
}

const TURN_ROLLOUT_SHARDS: usize = 64;
const TURN_ROLLOUT_SETS_PER_SHARD: usize = 32;
const TURN_ROLLOUT_WAYS: usize = 2;
const TURN_ROLLOUT_SLOTS_PER_SHARD: usize = TURN_ROLLOUT_SETS_PER_SHARD * TURN_ROLLOUT_WAYS;
const TURN_ROLLOUT_SLOT_NONE: u8 = 1;
const TURN_ROLLOUT_SLOT_SOME: u8 = 2;
const ROLLOUT_PILOT_VALID: u8 = 1 << 0;
const ROLLOUT_PILOT_WON: u8 = 1 << 1;
const ROLLOUT_PILOT_LINEAR_ADJ: u8 = 1 << 2;
const ROLLOUT_PILOT_DEFEAT: u8 = 1 << 3;
const ROLLOUT_PILOT_EVENT_DEFEAT: u8 = 1 << 4;

#[inline]
fn turn_rollout_cfg_hash(profile: &SolverSearchProfile, horizon_turns: usize) -> u64 {
    let flags_word = (horizon_turns as u64)
        | (u64::from(profile.base_score_only) << 16);
    let (pert_term, pert_scale) = match profile.beam_weight_perturbation {
        None => (0u64, 0u64),
        Some(p) => {
            let term = match p.term {
                BeamWeightTerm::CurrentEnergy => 1u64,
                BeamWeightTerm::PersistentBuffDelta => 2u64,
                BeamWeightTerm::EnemyHp => 3u64,
            };
            (term, p.scale.to_bits())
        }
    };
    let h = mix_cfg_u64(0x9E37_79B9_7F4A_7C15, flags_word | (pert_term << 20));
    mix_cfg_u64(h, pert_scale)
}

#[inline]
fn turn_rollout_cfg_hash_with_policy(
    profile: &SolverSearchProfile,
    horizon_turns: usize,
    brightest_flame_limit: Option<i32>,
    has_any_strategy_targets: bool,
) -> u64 {
    let base = turn_rollout_cfg_hash(profile, horizon_turns);
    let extra = match brightest_flame_limit {
        None => u64::from(has_any_strategy_targets),
        Some(lim) => (1u64 << 1) | ((u64::from(lim as u32)) << 8) | u64::from(has_any_strategy_targets),
    };
    mix_cfg_u64(base, extra)
}

#[derive(Clone, Copy)]
pub(crate) struct TurnRolloutPilotEntry {
    pub scores_at_zero: [f64; 4],
    pub goal_credit: i32,
    pub goal_count: i32,
    pub end_turn: u16,
    pub death_save_use_count: u8,
    pub alchemize_count: u8,
    pub empty_potion_slots: u8,
    pub flags: u8,
}

impl TurnRolloutPilotEntry {
    const EMPTY: Self = Self {
        scores_at_zero: [0.0; 4],
        goal_credit: 0,
        goal_count: 0,
        end_turn: 0,
        death_save_use_count: 0,
        alchemize_count: 0,
        empty_potion_slots: 0,
        flags: 0,
    };

    #[cfg(test)]
    #[inline(always)]
    pub(crate) fn from_eval(
        fast: &FastEvalSummary,
        full: &FullEvalDetails,
        root_baseline: &RootEvalBaseline,
        profile: &SolverSearchProfile,
        total_steps: usize,
    ) -> Self {
        let turn_num = fast.combat_ended_turn.unwrap_or(root_baseline.start_turn_number);
        Self::from_rollout_fields(
            fast,
            full,
            root_baseline,
            profile,
            total_steps,
            turn_num,
            false,
            fast.death_save_use_count,
            fast.death_save_hp_restored,
        )
    }

    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_rollout_state(
        state: &BattleState,
        fast: &FastEvalSummary,
        full: &FullEvalDetails,
        root_baseline: &RootEvalBaseline,
        profile: &SolverSearchProfile,
        total_steps: usize,
    ) -> Self {
        let death_save_use_count = fast
            .death_save_use_count
            .max(i32::from(state.player.death_save_use_count));
        let death_save_hp_restored = fast.death_save_hp_restored.max(
            state
                .player
                .death_save_relic_hp_restored
                .saturating_add(state.player.death_save_potion_hp_restored),
        );
        Self::from_rollout_fields(
            fast,
            full,
            root_baseline,
            profile,
            total_steps,
            state.player.turn_number,
            state.battleworn_dummy_timed_out,
            death_save_use_count,
            death_save_hp_restored,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn from_rollout_fields(
        fast: &FastEvalSummary,
        full: &FullEvalDetails,
        root_baseline: &RootEvalBaseline,
        profile: &SolverSearchProfile,
        total_steps: usize,
        turn_number: i32,
        battleworn_dummy_timed_out: bool,
        death_save_use_count: i32,
        death_save_hp_restored: i32,
    ) -> Self {
        let is_real_victory = fast.won && !battleworn_dummy_timed_out && !fast.is_event_defeat;
        let is_defeat = !is_real_victory && (fast.player_dead || fast.projected_player_hp <= 0);
        let is_event_defeat = !is_real_victory && !is_defeat && fast.is_event_defeat;
        let has_any_strategy_targets =
            root_baseline.effective_has_growth_targets || !root_baseline.active_relic_targets.is_empty();
        let all_growth_sat = !root_baseline.effective_has_growth_targets
            || root_baseline
                .growth_opportunity_targets
                .is_satisfied_by(&fast.growth_rewards);
        let all_relic_sat = root_baseline.active_relic_targets.is_empty()
            || root_baseline.relic_targets_satisfied(&fast.relic_eval);
        let all_strategy_targets_satisfied =
            fast.all_strategy_targets_satisfied || (all_growth_sat && all_relic_sat);

        let ds_u8 = death_save_use_count.clamp(0, u8::MAX as i32) as u8;
        let end_turn_u16 = turn_number.clamp(0, 4095) as u16;
        let mut flags = ROLLOUT_PILOT_VALID;

        let scores_at_zero = if is_real_victory {
            flags |= ROLLOUT_PILOT_WON;
            let post_combat_heal = ActEndingBossPolicy::ranked_post_combat_relic_heal(
                root_baseline.post_combat_relic_heal,
                true,
                fast.player_hp,
                fast.player_max_hp,
            );
            let net_hp_delta_centi = ActEndingBossPolicy::compute_net_persistent_hp_delta_centi(
                fast.player_hp,
                post_combat_heal,
                root_baseline.initial_player_hp,
                fast.player_max_hp,
                root_baseline.initial_player_max_hp,
                death_save_hp_restored,
                fast.strategic_hp_credit,
                root_baseline.boss_hp_relief,
                root_baseline.ignore_long_term_rewards,
                i32::from(fast.feed_count) * root_baseline.feed_max_hp_per_kill,
            );
            let turn_pen = i64::from(turn_number) * 5_000;
            let ds_pen = i64::from(death_save_use_count) * 4_000_000;
            let hp_term = i64::from(net_hp_delta_centi) * 390;
            let (raw_win_at_zero, floor) = if !has_any_strategy_targets {
                (
                    25_000_000 + hp_term - turn_pen - ds_pen,
                    if death_save_use_count == 0 {
                        12_000_000
                    } else {
                        8_000_000
                    },
                )
            } else if all_strategy_targets_satisfied {
                (
                    25_000_000
                        + hp_term
                        + i64::from(fast.strategy_goal_count) * 25_000
                        - turn_pen
                        - ds_pen,
                    if death_save_use_count == 0 {
                        12_000_000
                    } else {
                        8_000_000
                    },
                )
            } else if fast.strategy_goal_count > 0 || fast.strategy_goal_hp_credit > 0 {
                (
                    16_000_000
                        + hp_term
                        + i64::from(fast.strategy_goal_count) * 25_000
                        - turn_pen
                        - ds_pen,
                    if death_save_use_count == 0 {
                        7_000_000
                    } else {
                        4_000_000
                    },
                )
            } else {
                (
                    800_000 + hp_term - turn_pen - ds_pen,
                    if death_save_use_count == 0 {
                        200_000
                    } else {
                        100_000
                    },
                )
            };
            [
                raw_win_at_zero as f64,
                floor as f64,
                net_hp_delta_centi as f64,
                0.0,
            ]
        } else if is_defeat || is_event_defeat {
            if is_event_defeat {
                flags |= ROLLOUT_PILOT_EVENT_DEFEAT;
            } else {
                flags |= ROLLOUT_PILOT_DEFEAT;
            }
            let event_delta = if is_event_defeat { 2_000_000 } else { 0 };
            let tail = event_delta
                + i64::from(turn_number.clamp(0, 30)) * 60_000
                - i64::from(fast.enemy_hp.clamp(0, 2000)) * 10_000;
            let normal_score = (-20_000_000 + tail) as f64;
            let rescue_score = (-12_000_000 + tail) as f64;
            [normal_score, normal_score, normal_score, rescue_score]
        } else {
            let linear_adj = !profile.base_score_only;
            if linear_adj {
                flags |= ROLLOUT_PILOT_LINEAR_ADJ;
            }
            let step_actions = total_steps.min(u16::MAX as usize) as u16;
            compute_scores_at_zero(fast, full, root_baseline, profile, linear_adj, step_actions)
        };

        Self {
            scores_at_zero,
            goal_credit: fast.strategy_goal_hp_credit,
            goal_count: fast.strategy_goal_count,
            end_turn: end_turn_u16,
            death_save_use_count: ds_u8,
            alchemize_count: fast.alchemize_count,
            empty_potion_slots: fast.empty_potion_slots,
            flags,
        }
    }

    #[inline(always)]
    pub(crate) fn is_valid(self) -> bool {
        (self.flags & ROLLOUT_PILOT_VALID) != 0
    }

    #[inline(always)]
    pub(crate) fn won(self) -> bool {
        (self.flags & ROLLOUT_PILOT_WON) != 0
    }

    #[inline(always)]
    pub(crate) fn outcome_rank(self) -> u8 {
        if (self.flags & ROLLOUT_PILOT_WON) != 0 {
            3
        } else if (self.flags & (ROLLOUT_PILOT_DEFEAT | ROLLOUT_PILOT_EVENT_DEFEAT)) == 0 {
            2
        } else if (self.flags & ROLLOUT_PILOT_EVENT_DEFEAT) != 0 {
            1
        } else {
            0
        }
    }

    #[inline(always)]
    pub(crate) fn materialize_outcome(
        self,
        root_baseline: &RootEvalBaseline,
        eval_mode: EvalMode,
        optional_paid_potion_cost: i32,
        optional_paid_potion_count: usize,
        parent_action_count: u16,
        has_unused_paid_potions: bool,
    ) -> (f64, i64) {
        if self.won() {
            let net_c = PotionUsePolicy::compute_net_potion_opportunity_cost_centi(
                optional_paid_potion_cost,
                optional_paid_potion_count as i32,
                self.alchemize_count as i32,
                self.empty_potion_slots as i32,
                root_baseline,
            );
            let v_centi = (self.scores_at_zero[2] as i64) - i64::from(net_c);
            let score = (self.scores_at_zero[0] - (net_c as f64) * 390.0)
                .max(self.scores_at_zero[1]);
            return (score, v_centi);
        }
        if (self.flags & (ROLLOUT_PILOT_DEFEAT | ROLLOUT_PILOT_EVENT_DEFEAT)) != 0 {
            let score = if eval_mode == EvalMode::DesperateRescue || has_unused_paid_potions {
                self.scores_at_zero[3]
            } else {
                self.scores_at_zero[eval_mode_index(eval_mode)]
            };
            return (score, 0);
        }
        let base = self.scores_at_zero[eval_mode_index(eval_mode)];
        let score = if (self.flags & ROLLOUT_PILOT_LINEAR_ADJ) != 0 {
            let net_c = PotionUsePolicy::compute_net_potion_opportunity_cost_centi(
                optional_paid_potion_cost,
                optional_paid_potion_count as i32,
                self.alchemize_count as i32,
                self.empty_potion_slots as i32,
                root_baseline,
            );
            base - (net_c as f64) * eval_mode_potion_weight(eval_mode) - (parent_action_count as f64) * 5.0
        } else {
            base
        };
        (score, 0)
    }

    #[cfg(test)]
    #[inline(always)]
    pub(crate) fn materialize_score(
        self,
        root_baseline: &RootEvalBaseline,
        eval_mode: EvalMode,
        optional_paid_potion_cost: i32,
        optional_paid_potion_count: usize,
        parent_action_count: u16,
    ) -> f64 {
        self.materialize_outcome(
            root_baseline,
            eval_mode,
            optional_paid_potion_cost,
            optional_paid_potion_count,
            parent_action_count,
            false,
        )
        .0
    }
}

#[derive(Clone, Copy)]
pub(crate) struct TurnRolloutEntry {
    pub a: TurnRolloutPilotEntry,
    pub b: TurnRolloutPilotEntry,
}

#[repr(C, align(64))]
struct TurnRolloutAtomicSlot {
    seq: AtomicU32,
    _pad: u32,
    words: [AtomicU64; 15],
}

const _: () = assert!(std::mem::size_of::<TurnRolloutAtomicSlot>() == 128);

impl TurnRolloutAtomicSlot {
    #[inline]
    const fn new() -> Self {
        Self {
            seq: AtomicU32::new(0),
            _pad: 0,
            words: [const { AtomicU64::new(0) }; 15],
        }
    }

    #[inline(always)]
    fn try_read(
        &self,
        lo: u64,
        hi: u64,
        cfg_hash: u64,
        epoch: u32,
    ) -> Option<(u32, Option<TurnRolloutEntry>)> {
        let seq1 = self.seq.load(Ordering::Acquire);
        if (seq1 & 1) != 0 || seq1 == 0 {
            return None;
        }
        let w0 = self.words[0].load(Ordering::Relaxed);
        let w1 = self.words[1].load(Ordering::Relaxed);
        let w2 = self.words[2].load(Ordering::Relaxed);
        let w3 = self.words[3].load(Ordering::Relaxed);
        let w4 = self.words[4].load(Ordering::Relaxed);
        let w5 = self.words[5].load(Ordering::Relaxed);
        let w6 = self.words[6].load(Ordering::Relaxed);
        let w7 = self.words[7].load(Ordering::Relaxed);
        let w8 = self.words[8].load(Ordering::Relaxed);
        let w9 = self.words[9].load(Ordering::Relaxed);
        let w10 = self.words[10].load(Ordering::Relaxed);
        let w11 = self.words[11].load(Ordering::Relaxed);
        let w12 = self.words[12].load(Ordering::Relaxed);
        let w13 = self.words[13].load(Ordering::Relaxed);
        let w14 = self.words[14].load(Ordering::Relaxed);
        fence(Ordering::Acquire);
        let seq2 = self.seq.load(Ordering::Relaxed);
        if seq1 != seq2 {
            return None;
        }
        let slot_epoch = w11 as u32;
        let slot_state = (w11 >> 56) as u8;
        if w0 == lo && w1 == hi && w2 == cfg_hash && slot_epoch == epoch && slot_state != 0 {
            if slot_state == TURN_ROLLOUT_SLOT_NONE {
                return Some((seq1, None));
            }
            return Some((
                seq1,
                Some(TurnRolloutEntry {
                    a: TurnRolloutPilotEntry {
                        scores_at_zero: [
                            f64::from_bits(w3),
                            f64::from_bits(w4),
                            f64::from_bits(w5),
                            f64::from_bits(w6),
                        ],
                        goal_credit: w13 as u32 as i32,
                        goal_count: (w13 >> 32) as u32 as i32,
                        end_turn: ((w12 >> 40) & 0xFFF) as u16,
                        death_save_use_count: (w12 >> 24) as u8,
                        alchemize_count: (w11 >> 32) as u8,
                        empty_potion_slots: (w11 >> 40) as u8,
                        flags: (w11 >> 48) as u8,
                    },
                    b: TurnRolloutPilotEntry {
                        scores_at_zero: [
                            f64::from_bits(w7),
                            f64::from_bits(w8),
                            f64::from_bits(w9),
                            f64::from_bits(w10),
                        ],
                        goal_credit: w14 as u32 as i32,
                        goal_count: (w14 >> 32) as u32 as i32,
                        end_turn: ((w12 >> 52) & 0xFFF) as u16,
                        death_save_use_count: (w12 >> 32) as u8,
                        alchemize_count: w12 as u8,
                        empty_potion_slots: (w12 >> 8) as u8,
                        flags: (w12 >> 16) as u8,
                    },
                }),
            ));
        }
        None
    }

    #[inline(always)]
    fn try_write(
        &self,
        expected_seq: u32,
        lo: u64,
        hi: u64,
        cfg_hash: u64,
        epoch: u32,
        res: Option<TurnRolloutEntry>,
    ) {
        if self
            .seq
            .compare_exchange(
                expected_seq,
                expected_seq.wrapping_add(1),
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .is_err()
        {
            return;
        }
        fence(Ordering::Release);
        self.words[0].store(lo, Ordering::Relaxed);
        self.words[1].store(hi, Ordering::Relaxed);
        self.words[2].store(cfg_hash, Ordering::Relaxed);
        match res {
            None => {
                for w in &self.words[3..11] {
                    w.store(0, Ordering::Relaxed);
                }
                self.words[11].store(
                    (epoch as u64) | (u64::from(TURN_ROLLOUT_SLOT_NONE) << 56),
                    Ordering::Relaxed,
                );
                self.words[12].store(0, Ordering::Relaxed);
                self.words[13].store(0, Ordering::Relaxed);
                self.words[14].store(0, Ordering::Relaxed);
            }
            Some(e) => {
                self.words[3].store(e.a.scores_at_zero[0].to_bits(), Ordering::Relaxed);
                self.words[4].store(e.a.scores_at_zero[1].to_bits(), Ordering::Relaxed);
                self.words[5].store(e.a.scores_at_zero[2].to_bits(), Ordering::Relaxed);
                self.words[6].store(e.a.scores_at_zero[3].to_bits(), Ordering::Relaxed);
                self.words[7].store(e.b.scores_at_zero[0].to_bits(), Ordering::Relaxed);
                self.words[8].store(e.b.scores_at_zero[1].to_bits(), Ordering::Relaxed);
                self.words[9].store(e.b.scores_at_zero[2].to_bits(), Ordering::Relaxed);
                self.words[10].store(e.b.scores_at_zero[3].to_bits(), Ordering::Relaxed);
                self.words[11].store(
                    (epoch as u64)
                        | (u64::from(e.a.alchemize_count) << 32)
                        | (u64::from(e.a.empty_potion_slots) << 40)
                        | (u64::from(e.a.flags) << 48)
                        | (u64::from(TURN_ROLLOUT_SLOT_SOME) << 56),
                    Ordering::Relaxed,
                );
                self.words[12].store(
                    u64::from(e.b.alchemize_count)
                        | (u64::from(e.b.empty_potion_slots) << 8)
                        | (u64::from(e.b.flags) << 16)
                        | (u64::from(e.a.death_save_use_count) << 24)
                        | (u64::from(e.b.death_save_use_count) << 32)
                        | ((u64::from(e.a.end_turn) & 0xFFF) << 40)
                        | ((u64::from(e.b.end_turn) & 0xFFF) << 52),
                    Ordering::Relaxed,
                );
                self.words[13].store(
                    ((e.a.goal_credit as u32) as u64) | (((e.a.goal_count as u32) as u64) << 32),
                    Ordering::Relaxed,
                );
                self.words[14].store(
                    ((e.b.goal_credit as u32) as u64) | (((e.b.goal_count as u32) as u64) << 32),
                    Ordering::Relaxed,
                );
            }
        }
        let ns = expected_seq.wrapping_add(2);
        self.seq
            .store(if ns == 0 { 2 } else { ns }, Ordering::Release);
    }
}

#[derive(Default)]
struct TurnRolloutShard {
    slots: OnceLock<Box<[TurnRolloutAtomicSlot; TURN_ROLLOUT_SLOTS_PER_SHARD]>>,
    victim_bits: AtomicU32,
}

impl TurnRolloutShard {
    #[cold]
    #[inline(never)]
    fn alloc_slots() -> Box<[TurnRolloutAtomicSlot; TURN_ROLLOUT_SLOTS_PER_SHARD]> {
        (0..TURN_ROLLOUT_SLOTS_PER_SHARD)
            .map(|_| TurnRolloutAtomicSlot::new())
            .collect::<Vec<_>>()
            .into_boxed_slice()
            .try_into()
            .unwrap_or_else(|_| unreachable!())
    }
}

pub(crate) struct TurnRolloutCache {
    shards: Box<[TurnRolloutShard]>,
    epoch: AtomicU32,
}

impl Default for TurnRolloutCache {
    fn default() -> Self {
        Self::new()
    }
}

impl TurnRolloutCache {
    pub(crate) fn new() -> Self {
        let shards = (0..TURN_ROLLOUT_SHARDS)
            .map(|_| TurnRolloutShard::default())
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            shards,
            epoch: AtomicU32::new(1),
        }
    }

    pub(crate) fn clear(&self) {
        let mut next = self.epoch.load(Ordering::Relaxed).wrapping_add(1);
        if next == 0 {
            next = 1;
            for shard in self.shards.iter() {
                if let Some(slots) = shard.slots.get() {
                    for slot in slots.iter() {
                        let seq = slot.seq.load(Ordering::Acquire);
                        if (seq & 1) == 0 && seq != 0 {
                            if slot
                                .seq
                                .compare_exchange(
                                    seq,
                                    seq.wrapping_add(1),
                                    Ordering::Acquire,
                                    Ordering::Relaxed,
                                )
                                .is_ok()
                            {
                                fence(Ordering::Release);
                                slot.words[11].store(0, Ordering::Relaxed);
                                let ns = seq.wrapping_add(2);
                                slot.seq
                                    .store(if ns == 0 { 2 } else { ns }, Ordering::Release);
                            }
                        }
                    }
                }
                shard.victim_bits.store(0, Ordering::Relaxed);
            }
        }
        self.epoch.store(next, Ordering::Release);
    }

    #[inline(always)]
    fn slot_indices(exact_state_hash: StateHash, cfg_hash: u64) -> (usize, usize) {
        let shard_idx = exact_state_hash.shard_index() & (TURN_ROLLOUT_SHARDS - 1);
        let lo = exact_state_hash.0 as usize;
        let set_idx =
            (lo ^ (lo >> 16) ^ (cfg_hash as usize)) & (TURN_ROLLOUT_SETS_PER_SHARD - 1);
        (shard_idx, set_idx)
    }

    #[inline]
    pub(crate) fn get(
        &self,
        exact_state_hash: StateHash,
        cfg_hash: u64,
    ) -> Option<Option<TurnRolloutEntry>> {
        let epoch = self.epoch.load(Ordering::Acquire);
        let (shard_idx, set_idx) = Self::slot_indices(exact_state_hash, cfg_hash);
        let slots = self.shards[shard_idx].slots.get()?;
        let lo = exact_state_hash.0 as u64;
        let hi = (exact_state_hash.0 >> 64) as u64;
        let base = set_idx * TURN_ROLLOUT_WAYS;
        for w in 0..TURN_ROLLOUT_WAYS {
            if let Some((_seq, entry)) = slots[base + w].try_read(lo, hi, cfg_hash, epoch) {
                return Some(entry);
            }
        }
        None
    }

    #[inline]
    pub(crate) fn insert(
        &self,
        exact_state_hash: StateHash,
        cfg_hash: u64,
        res: Option<TurnRolloutEntry>,
    ) {
        let epoch = self.epoch.load(Ordering::Acquire);
        let (shard_idx, set_idx) = Self::slot_indices(exact_state_hash, cfg_hash);
        let shard = &self.shards[shard_idx];
        let slots = shard.slots.get_or_init(TurnRolloutShard::alloc_slots);
        let lo = exact_state_hash.0 as u64;
        let hi = (exact_state_hash.0 >> 64) as u64;
        let base = set_idx * TURN_ROLLOUT_WAYS;

        let mut target: Option<(usize, u32)> = None;
        let mut empty_way: Option<(usize, u32)> = None;
        for w in 0..TURN_ROLLOUT_WAYS {
            let s = &slots[base + w];
            let seq1 = s.seq.load(Ordering::Acquire);
            if seq1 == 0 {
                if empty_way.is_none() {
                    empty_way = Some((w, 0));
                }
                continue;
            }
            if (seq1 & 1) != 0 {
                continue;
            }
            let w0 = s.words[0].load(Ordering::Relaxed);
            let w1 = s.words[1].load(Ordering::Relaxed);
            let w2 = s.words[2].load(Ordering::Relaxed);
            let w11 = s.words[11].load(Ordering::Relaxed);
            fence(Ordering::Acquire);
            let seq2 = s.seq.load(Ordering::Relaxed);
            if seq1 != seq2 {
                continue;
            }
            let slot_epoch = w11 as u32;
            let slot_state = (w11 >> 56) as u8;
            if slot_epoch != epoch || slot_state == 0 {
                if empty_way.is_none() {
                    empty_way = Some((w, seq1));
                }
            } else if w0 == lo && w1 == hi && w2 == cfg_hash {
                target = Some((w, seq1));
                break;
            }
        }

        let (way, expected_seq) = if let Some(hit) = target {
            hit
        } else if let Some(ew) = empty_way {
            ew
        } else {
            let bit = 1u32 << set_idx;
            let prev = shard.victim_bits.fetch_xor(bit, Ordering::Relaxed);
            let w = usize::from((prev & bit) != 0);
            let seq = slots[base + w].seq.load(Ordering::Acquire);
            if (seq & 1) != 0 {
                return;
            }
            (w, seq)
        };

        slots[base + way].try_write(expected_seq, lo, hi, cfg_hash, epoch, res);
    }
}

#[derive(Clone, Copy)]
pub(crate) struct PlayableRolloutCard {
    handle: CardHandle,
    card_id: CardId,
    energy_cost: i32,
    star_cost: i32,
    upg: bool,
    exhausts: bool,
    is_retained: bool,
}

struct ChoiceProbeL1Table {
    tags: Box<[[u64; CHOICE_PROBE_L1_WAYS]; CHOICE_PROBE_L1_SETS]>,
    slots: Box<[ChoiceProbeSlot; CHOICE_PROBE_L1_SLOTS]>,
    cursor: Box<[u8; CHOICE_PROBE_L1_SETS]>,
}

impl ChoiceProbeL1Table {
    #[cold]
    #[inline(never)]
    fn alloc() -> Self {
        Self {
            tags: vec![[0u64; CHOICE_PROBE_L1_WAYS]; CHOICE_PROBE_L1_SETS]
                .into_boxed_slice()
                .try_into()
                .unwrap_or_else(|_| unreachable!()),
            slots: vec![ChoiceProbeSlot::EMPTY; CHOICE_PROBE_L1_SLOTS]
                .into_boxed_slice()
                .try_into()
                .unwrap_or_else(|_| unreachable!()),
            cursor: vec![0u8; CHOICE_PROBE_L1_SETS]
                .into_boxed_slice()
                .try_into()
                .unwrap_or_else(|_| unreachable!()),
        }
    }
}

pub(crate) struct ChoiceProbeL1 {
    pub(crate) use_l1: bool,
    table: Option<ChoiceProbeL1Table>,
    epoch: u32,
}

impl ChoiceProbeL1 {
    pub(crate) fn new() -> Self {
        Self {
            use_l1: false,
            table: None,
            epoch: 0,
        }
    }
}

pub(crate) struct ChoiceProbeWorkspace {
    pub(crate) probe_state: BattleState,
    pub(crate) compact_steps: Vec<CompactRolloutStep>,
    pub(crate) choice_slab: RolloutChoiceSlab,
    pub(crate) sel_buf: SmallVec<[usize; 16]>,
    pub(crate) intent_cache: IntentHitsCache,
    pub(crate) blocked_buf: SmallVec<[CardHandle; 8]>,
    pub(crate) empty_q: SmallVec<[Vec<usize>; 2]>,
    pub(crate) playable_buf: Vec<PlayableRolloutCard>,
    pub(crate) hand_buf: Vec<CardHandle>,
    pub(crate) seen_keys: Vec<RolloutDedupKey>,
    pub(crate) choice_scored_buf: Vec<(usize, i32, u32)>,
    pub(crate) l1: ChoiceProbeL1,
}

impl ChoiceProbeWorkspace {
    pub(crate) fn new(static_ctx: Arc<RootStaticContext>) -> Self {
        Self {
            probe_state: empty_scratch_state(static_ctx),
            compact_steps: Vec::with_capacity(32),
            choice_slab: RolloutChoiceSlab::default(),
            sel_buf: SmallVec::new(),
            intent_cache: IntentHitsCache::new(),
            blocked_buf: SmallVec::new(),
            empty_q: SmallVec::new(),
            playable_buf: Vec::with_capacity(16),
            hand_buf: Vec::with_capacity(16),
            seen_keys: Vec::with_capacity(16),
            choice_scored_buf: Vec::with_capacity(32),
            l1: ChoiceProbeL1::new(),
        }
    }
}

pub(crate) struct RolloutWorkspace {
    pub(crate) base_state: BattleState,
    pub(crate) pilot_state: BattleState,
    pub(crate) steps_a: Vec<CompactRolloutStep>,
    pub(crate) choices_a: RolloutChoiceSlab,
    pub(crate) steps_b: Vec<CompactRolloutStep>,
    pub(crate) choices_b: RolloutChoiceSlab,
    pub(crate) sel_buf: SmallVec<[usize; 16]>,
    pub(crate) intent_cache: IntentHitsCache,
    pub(crate) blocked_buf: SmallVec<[CardHandle; 8]>,
    pub(crate) empty_q: SmallVec<[Vec<usize>; 2]>,
    pub(crate) playable_buf: Vec<PlayableRolloutCard>,
    pub(crate) hand_buf: Vec<CardHandle>,
    pub(crate) seen_keys: Vec<RolloutDedupKey>,
    pub(crate) choice_scored_buf: Vec<(usize, i32, u32)>,
    pub(crate) results_buf: Vec<(usize, Option<RolloutResult>)>,
}

impl RolloutWorkspace {
    pub(crate) fn new(static_ctx: Arc<RootStaticContext>) -> Self {
        Self {
            base_state: empty_scratch_state(static_ctx.clone()),
            pilot_state: empty_scratch_state(static_ctx),
            steps_a: Vec::with_capacity(32),
            choices_a: RolloutChoiceSlab {
                steps_meta: SmallVec::with_capacity(32),
                indices: SmallVec::with_capacity(64),
            },
            steps_b: Vec::with_capacity(32),
            choices_b: RolloutChoiceSlab {
                steps_meta: SmallVec::with_capacity(32),
                indices: SmallVec::with_capacity(64),
            },
            sel_buf: SmallVec::new(),
            intent_cache: IntentHitsCache::new(),
            blocked_buf: SmallVec::new(),
            empty_q: SmallVec::new(),
            playable_buf: Vec::with_capacity(16),
            hand_buf: Vec::with_capacity(16),
            seen_keys: Vec::with_capacity(16),
            choice_scored_buf: Vec::with_capacity(32),
            results_buf: Vec::with_capacity(32),
        }
    }
}

#[derive(Clone, Copy)]
struct FastEnemySlot {
    handle: CreatureHandle,
    hp: i32,
    eff_hp: i32,
    incoming: i32,
    target_bonus: i32,
    vuln: bool,
}

struct FastTurnContext {
    player_hp: i32,
    player_max_hp: i32,
    player_block: i32,
    missing_hp: i32,
    player_energy: i32,
    player_str: i32,
    player_dex: i32,
    player_weak: bool,
    player_frail: bool,
    player_has_fan_of_knives: bool,
    player_has_seeking_edge: bool,
    has_active_scaling: bool,
    has_retain_hand: bool,
    preserves_excess_block: bool,
    any_enemy_vuln: bool,
    unblocked_incoming: i32,
    alive_enemies: i32,
    reviving_enemies: i32,
    enemy_count: usize,
    best_non_attack_enemy_slot: Option<usize>,
    best_debuff_enemy_slot: Option<usize>,
    enemies: [std::mem::MaybeUninit<FastEnemySlot>; MAX_FAST_ENEMIES],
}

impl FastTurnContext {
    #[inline(always)]
    fn enemy_slot(&self, slot: usize) -> FastEnemySlot {
        debug_assert!(slot < self.enemy_count);
        unsafe { self.enemies.get_unchecked(slot).assume_init() }
    }

    #[inline(always)]
    fn enemy_handle(&self, slot: usize) -> CreatureHandle {
        self.enemy_slot(slot).handle
    }

    #[inline(always)]
    fn enemy_hp(&self, slot: usize) -> i32 {
        self.enemy_slot(slot).hp
    }

    #[inline(always)]
    fn enemy_incoming(&self, slot: usize) -> i32 {
        self.enemy_slot(slot).incoming
    }

    #[inline(always)]
    fn enemy_target_bonus(&self, slot: usize) -> i32 {
        self.enemy_slot(slot).target_bonus
    }

    #[inline(always)]
    fn enemy_vuln(&self, slot: usize) -> bool {
        self.enemy_slot(slot).vuln
    }
}

#[inline(always)]
fn has_retain_hand_or_pyramid(state: &BattleState) -> bool {
    if state
        .player
        .relics
        .iter()
        .any(|r| !r.is_melted && r.id == RelicId::RunicPyramid)
    {
        return true;
    }
    state.player_creature().is_some_and(|pc| {
        pc.powers.iter().any(|p| {
            p.amount > 0
                && matches!(p.id, PowerId::RetainHandPower | PowerId::WellLaidPlansPower)
        })
    })
}

#[inline(always)]
fn compute_fast_turn_context<const IS_SETUP: bool>(
    state: &BattleState,
    content: &dyn ContentDispatcher,
    root: &RootEvalBaseline,
    intent_cache: &mut IntentHitsCache,
) -> FastTurnContext {
    let mut out = std::mem::MaybeUninit::<FastTurnContext>::uninit();
    compute_fast_turn_context_into::<IS_SETUP>(state, content, root, intent_cache, &mut out);
    unsafe { out.assume_init() }
}

fn compute_fast_turn_context_into<'a, const IS_SETUP: bool>(
    state: &BattleState,
    content: &dyn ContentDispatcher,
    root: &RootEvalBaseline,
    intent_cache: &mut IntentHitsCache,
    out: &'a mut std::mem::MaybeUninit<FastTurnContext>,
) -> &'a FastTurnContext {
    let creatures_slice = state.creatures.as_slice();
    let creatures_contiguous = state.handle_layout.creatures;
    let get_creature = |h: CreatureHandle| -> Option<&sts2_kernel::state::CreatureInst> {
        let idx = h.0.wrapping_sub(1) as usize;
        if creatures_contiguous {
            if idx < creatures_slice.len() {
                let c = unsafe { creatures_slice.get_unchecked(idx) };
                #[cfg(debug_assertions)]
                debug_assert_eq!(c.handle, h);
                Some(c)
            } else {
                None
            }
        } else {
            state.creature(h)
        }
    };
    let pc = get_creature(state.player.creature_handle);
    let player_hp = pc.map_or(state.player.current_hp, |c| c.current_hp);
    let player_max_hp = pc.map_or(state.player.max_hp, |c| c.max_hp);
    let player_block = pc.map_or(state.player.block, |c| c.block).max(0);

    let mut player_str = 0i32;
    let mut player_dex = 0i32;
    let mut player_focus = 0i32;
    let mut player_vuln = 0i32;
    let mut player_weak = false;
    let mut player_frail = false;
    let mut player_has_fan_of_knives = false;
    let mut player_has_seeking_edge = false;
    let mut player_intangible = false;
    let mut player_accelerant = 0i32;
    let mut player_end_turn_block = 0i32;
    let mut has_active_scaling = false;
    let mut has_retain_hand = state
        .player
        .relics
        .iter()
        .any(|r| !r.is_melted && r.id == RelicId::RunicPyramid);
    let mut preserves_excess_block = false;

    if let Some(pcr) = pc {
        let tables = if IS_SETUP {
            state.hook_cache.cached_tables
        } else {
            None
        };
        for p in &pcr.powers {
            if p.amount == 0 {
                continue;
            }
            match p.id {
                PowerId::StrengthPower => player_str += p.amount,
                PowerId::DexterityPower => player_dex += p.amount,
                PowerId::FocusPower => player_focus += p.amount,
                PowerId::VulnerablePower if p.amount > 0 => player_vuln += p.amount,
                PowerId::WeakPower if p.amount > 0 => player_weak = true,
                PowerId::FrailPower if p.amount > 0 => player_frail = true,
                PowerId::FanOfKnivesPower if p.amount > 0 => player_has_fan_of_knives = true,
                PowerId::SeekingEdgePower if p.amount > 0 => player_has_seeking_edge = true,
                PowerId::IntangiblePower if p.amount > 0 => player_intangible = true,
                PowerId::AccelerantPower => player_accelerant += p.amount,
                PowerId::PlatingPower if p.amount > 0 => player_end_turn_block += p.amount,
                PowerId::BarricadePower | PowerId::BlurPower => preserves_excess_block = true,
                PowerId::RetainHandPower | PowerId::WellLaidPlansPower if p.amount > 0 => {
                    has_retain_hand = true;
                }
                _ => {}
            }
            if IS_SETUP && !has_active_scaling && p.amount > 0 {
                let pm_res = match tables.and_then(|t| t.power_metas.get(p.id as usize)) {
                    Some(pm) => Ok(pm),
                    None => content.power_meta(p.id),
                };
                if pm_res.is_ok_and(|pm| pm.power_type != PowerType::Debuff) {
                    has_active_scaling = true;
                }
            }
        }
    }

    let has_cables = root.has_gold_plated_cables;
    let mut frost_passive_block = 0i32;
    if !state.player.orbs.is_empty() {
        if IS_SETUP {
            has_active_scaling = true;
        }
        for (o_idx, orb) in state.player.orbs.iter().enumerate() {
            if orb.id == OrbId::FrostOrb {
                let single = (2 + player_focus).max(0);
                let mult = if o_idx == 0 && has_cables { 2 } else { 1 };
                frost_passive_block += single * mult;
            }
        }
    }

    let (osty_hp, wuppy_hp) = if state.pet_handles.is_empty() {
        (0, 0)
    } else {
        let osty = state
            .osty_handle()
            .and_then(get_creature)
            .filter(|c| c.is_alive())
            .map_or(0, |c| c.current_hp.max(0));
        let wuppy: i32 = state
            .pet_handles
            .iter()
            .filter_map(|&h| get_creature(h))
            .filter(|c| c.is_alive() && c.monster_id == Some(MonsterId::Wuppy))
            .map(|c| c.current_hp.max(0))
            .sum();
        (osty, wuppy)
    };

    let ptr = out.as_mut_ptr();
    let mut any_enemy_vuln = false;
    let mut alive_enemies = 0i32;
    let mut reviving_enemies = 0i32;
    let mut enemy_count = 0usize;
    let mut best_non_attack_enemy_slot: Option<usize> = None;
    let mut best_debuff_enemy_slot: Option<usize> = None;

    let mut total_incoming = 0i32;
    let mut best_non_atk_bonus = i32::MIN;
    let mut best_deb_score = i32::MIN;
    let multi_enemy_target_cache = !IS_SETUP && state.enemy_handles.len() > 1;

    for &eh in &state.enemy_handles {
        let Some(ec) = get_creature(eh) else {
            continue;
        };
        if ec.is_dead() {
            if is_creature_reviving(state, ec) {
                reviving_enemies += 1;
            }
            continue;
        }
        if !ec.is_alive() {
            continue;
        }

        alive_enemies += 1;
        let cur_hp = ec.current_hp.max(0);
        let cur_block = ec.block.max(0);
        let mut enemy_str = 0i32;
        let mut enemy_weak = false;
        let mut enemy_poison = 0i32;
        let mut enemy_vuln = false;
        let mut has_reattach = false;
        let mut has_damage_cap = false;

        for p in &ec.powers {
            match p.id {
                PowerId::StrengthPower => enemy_str += p.amount,
                PowerId::WeakPower if p.amount > 0 => enemy_weak = true,
                PowerId::VulnerablePower if p.amount > 0 => enemy_vuln = true,
                PowerId::PoisonPower if p.amount > 0 => enemy_poison += p.amount,
                PowerId::ReattachPower if p.amount > 0 => has_reattach = true,
                PowerId::IntangiblePower
                | PowerId::BufferPower
                | PowerId::SlipperyPower
                | PowerId::HardToKillPower
                | PowerId::HardenedShellPower
                    if p.amount > 0 =>
                {
                    has_damage_cap = true;
                }
                _ => {}
            }
        }

        let ticks = enemy_poison.min(1 + player_accelerant.max(0));
        let poison_dmg = if ticks > 0 {
            ticks * (2 * enemy_poison - ticks + 1) / 2
        } else {
            0
        };
        let poison_lethal = cur_hp > 0 && poison_dmg >= cur_hp && !has_reattach && !has_damage_cap;
        let mut inc = 0i32;
        if !poison_lethal {
            let hits = intent_cache.lookup_i32(state, content, eh, ec);
            for &(base_dmg, count) in hits {
                let mut per_hit = (base_dmg + enemy_str).max(0);
                if enemy_weak {
                    per_hit = if root.has_paper_krane {
                        (per_hit * 60) / 100
                    } else {
                        let res = (per_hit * 3) >> 2;
                        #[cfg(debug_assertions)]
                        debug_assert_eq!(res, (per_hit * 75) / 100);
                        res
                    };
                }
                if player_vuln > 0 {
                    per_hit = if root.has_odd_mushroom {
                        let res = (per_hit * 5) >> 2;
                        #[cfg(debug_assertions)]
                        debug_assert_eq!(res, (per_hit * 125) / 100);
                        res
                    } else {
                        let res = (per_hit * 3) >> 1;
                        #[cfg(debug_assertions)]
                        debug_assert_eq!(res, (per_hit * 150) / 100);
                        res
                    };
                }
                if player_intangible && per_hit > 1 {
                    per_hit = 1;
                }
                inc += per_hit.max(0) * count.max(0);
            }
        }
        total_incoming += inc;

        if enemy_count < MAX_FAST_ENEMIES {
            let idx = enemy_count;
            let t_bonus = if cur_hp > 0 {
                (200 - cur_hp.min(200)).max(0) + if inc > 0 { 60 } else { 0 }
            } else {
                0
            };
            unsafe {
                core::ptr::addr_of_mut!((*ptr).enemies)
                    .cast::<std::mem::MaybeUninit<FastEnemySlot>>()
                    .add(idx)
                    .write(std::mem::MaybeUninit::new(FastEnemySlot {
                        handle: eh,
                        hp: cur_hp,
                        eff_hp: cur_hp + cur_block,
                        incoming: inc,
                        target_bonus: t_bonus,
                        vuln: enemy_vuln,
                    }));
            }
            any_enemy_vuln |= enemy_vuln;
            if multi_enemy_target_cache && cur_hp > 0 {
                if t_bonus > best_non_atk_bonus {
                    best_non_atk_bonus = t_bonus;
                    best_non_attack_enemy_slot = Some(idx);
                }
                let deb_sc = (if inc > 0 { 420 } else { 180 }) + t_bonus;
                if deb_sc > best_deb_score {
                    best_deb_score = deb_sc;
                    best_debuff_enemy_slot = Some(idx);
                }
            }
            enemy_count += 1;
        }
    }

    let defense = player_block + player_end_turn_block + frost_passive_block + osty_hp + wuppy_hp;
    let unblocked_incoming = (total_incoming - defense).max(0);
    unsafe {
        use core::ptr::addr_of_mut;
        addr_of_mut!((*ptr).player_hp).write(player_hp);
        addr_of_mut!((*ptr).player_max_hp).write(player_max_hp);
        addr_of_mut!((*ptr).player_block).write(player_block);
        addr_of_mut!((*ptr).missing_hp).write((player_max_hp - player_hp).max(0));
        addr_of_mut!((*ptr).player_energy).write(state.player.energy);
        addr_of_mut!((*ptr).player_str).write(player_str);
        addr_of_mut!((*ptr).player_dex).write(player_dex);
        addr_of_mut!((*ptr).player_weak).write(player_weak);
        addr_of_mut!((*ptr).player_frail).write(player_frail);
        addr_of_mut!((*ptr).player_has_fan_of_knives).write(player_has_fan_of_knives);
        addr_of_mut!((*ptr).player_has_seeking_edge).write(player_has_seeking_edge);
        addr_of_mut!((*ptr).has_active_scaling).write(has_active_scaling);
        addr_of_mut!((*ptr).has_retain_hand).write(has_retain_hand);
        addr_of_mut!((*ptr).preserves_excess_block).write(preserves_excess_block);
        addr_of_mut!((*ptr).any_enemy_vuln).write(any_enemy_vuln);
        addr_of_mut!((*ptr).unblocked_incoming).write(unblocked_incoming);
        addr_of_mut!((*ptr).alive_enemies).write(alive_enemies);
        addr_of_mut!((*ptr).reviving_enemies).write(reviving_enemies);
        addr_of_mut!((*ptr).enemy_count).write(enemy_count);
        addr_of_mut!((*ptr).best_non_attack_enemy_slot).write(best_non_attack_enemy_slot);
        addr_of_mut!((*ptr).best_debuff_enemy_slot).write(best_debuff_enemy_slot);
        out.assume_init_ref()
    }
}

#[inline]
fn parent_satisfies_hard_potions(
    arena: &NodeArena,
    parent_id: NodeId,
    root: &RootEvalBaseline,
    policy: &SearchPolicy,
) -> bool {
    let pnode = arena.get(parent_id);
    if (pnode.forced_used_mask & root.all_forced_potion_mask) != root.all_forced_potion_mask {
        return false;
    }
    if (pnode.explicit_potion_count as usize) < root.minimum_required_potion_uses {
        return false;
    }
    if policy.potion_policy == SolverPotionPolicy::Disabled {
        let allowed_forced =
            (pnode.forced_used_mask & root.all_forced_potion_mask).count_ones() as u16;
        if u16::from(pnode.explicit_potion_count) > allowed_forced {
            return false;
        }
    }
    true
}

#[inline(always)]
fn count_non_empty_potion_slots(state: &BattleState) -> usize {
    let mut n = 0usize;
    for s in &state.player.potion_slots {
        n += usize::from(s.is_some());
    }
    n
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FastEnergyHook {
    None,
    BorrowedTime(i32),
    Veilpiercer,
    Corruption,
    FreeAttack,
    FreeSkill,
    FreePower,
    Fallback,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FastShouldPlayHook {
    None,
    ChainsOfBinding { bound_played: bool },
    Fallback,
}

#[inline]
fn resolve_sole_player_power_in_slot<'a>(
    state: &'a BattleState,
    slot: HookSlot,
) -> Option<&'a sts2_kernel::state::PowerInst> {
    let words = state.hook_cache.flat_slot_words as usize;
    if words == 0 {
        return None;
    }
    let mut single_listener: Option<CachedHookListener> = None;
    for lw in 0..words {
        let bits = state.hook_cache.slot_listener_mask_word(slot as usize, lw);
        if bits == 0 {
            continue;
        }
        if (bits & (bits - 1)) != 0 || single_listener.is_some() {
            return None;
        }
        let idx = (lw << 6) + bits.trailing_zeros() as usize;
        single_listener = Some(state.hook_cache.flat_listeners[idx]);
    }
    if let Some(CachedHookListener::Power {
        creature,
        creature_idx,
        power_idx,
        ..
    }) = single_listener
    {
        if creature == state.player.creature_handle
            && creature_idx != u16::MAX
            && power_idx != u16::MAX
        {
            return state
                .creatures
                .get(creature_idx as usize)
                .and_then(|c| c.powers.get(power_idx as usize));
        }
    }
    None
}

#[inline(always)]
fn resolve_fast_query_and_hooks(
    state: &mut BattleState,
    content: &dyn ContentDispatcher,
) -> (bool, sts2_types::HookMask, FastEnergyHook, FastShouldPlayHook) {
    let cache_ready = state.hook_cache.masks_are_valid();
    let comb = state.hook_cache.masks.combat_combined;
    let kw_ok = !comb.contains(HookSlot::TryModifyKeywordsInCombat)
        || state
            .hook_cache
            .cached_tables
            .is_some_and(|t| t.keywords_in_combat_known_only);
    if !cache_ready || !kw_ok || comb.contains(HookSlot::TryModifyStarCostInCombat) {
        return (
            false,
            comb,
            FastEnergyHook::Fallback,
            FastShouldPlayHook::Fallback,
        );
    }
    let has_should_play = comb.contains(HookSlot::ShouldPlay);
    let has_early = comb.contains(HookSlot::TryModifyEnergyCostInCombat);
    let has_late = comb.contains(HookSlot::TryModifyEnergyCostInCombatLate);
    if !has_should_play && !has_early && !has_late {
        return (
            true,
            comb,
            FastEnergyHook::None,
            FastShouldPlayHook::None,
        );
    }
    if state.hook_cache.dirty_mask != 0 {
        let _ = state.ensure_hook_cache(content);
    }
    let comb = state.hook_cache.masks.combat_combined;
    let fast_should_play = if !comb.contains(HookSlot::ShouldPlay) {
        FastShouldPlayHook::None
    } else if let Some(pw) = resolve_sole_player_power_in_slot(state, HookSlot::ShouldPlay) {
        if pw.id == PowerId::ChainsOfBindingPower {
            let bound_played = pw
                .ext_index
                .and_then(|e| state.power_extensions.get(e as usize))
                .and_then(|ext| ext.private_fields.get_bool_static(hot_keys::CHAINS_OF_BINDING_BOUND_PLAYED))
                .unwrap_or(false);
            FastShouldPlayHook::ChainsOfBinding { bound_played }
        } else {
            return (
                false,
                comb,
                FastEnergyHook::Fallback,
                FastShouldPlayHook::Fallback,
            );
        }
    } else {
        return (
            false,
            comb,
            FastEnergyHook::Fallback,
            FastShouldPlayHook::Fallback,
        );
    };

    let has_early = comb.contains(HookSlot::TryModifyEnergyCostInCombat);
    let has_late = comb.contains(HookSlot::TryModifyEnergyCostInCombatLate);
    let fast_energy_hook = if !has_early && !has_late {
        FastEnergyHook::None
    } else if has_early && !has_late {
        if let Some(pw) =
            resolve_sole_player_power_in_slot(state, HookSlot::TryModifyEnergyCostInCombat)
        {
            if pw.id == PowerId::BorrowedTimePower {
                FastEnergyHook::BorrowedTime(pw.amount)
            } else {
                FastEnergyHook::Fallback
            }
        } else {
            FastEnergyHook::Fallback
        }
    } else if !has_early && has_late {
        if let Some(pw) =
            resolve_sole_player_power_in_slot(state, HookSlot::TryModifyEnergyCostInCombatLate)
        {
            match pw.id {
                PowerId::VeilpiercerPower => FastEnergyHook::Veilpiercer,
                PowerId::CorruptionPower => FastEnergyHook::Corruption,
                PowerId::FreeAttackPower => FastEnergyHook::FreeAttack,
                PowerId::FreeSkillPower => FastEnergyHook::FreeSkill,
                PowerId::FreePowerPower => FastEnergyHook::FreePower,
                _ => FastEnergyHook::Fallback,
            }
        } else {
            FastEnergyHook::Fallback
        }
    } else {
        FastEnergyHook::Fallback
    };
    (true, comb, fast_energy_hook, fast_should_play)
}

#[inline(never)]
fn resolve_fast_energy_hook_cost(
    state: &mut BattleState,
    content: &dyn ContentDispatcher,
    comb: sts2_types::HookMask,
    ch: CardHandle,
    local_e: i32,
    card_type: CardType,
    kw_base: CardKeywords,
    has_hex_power: bool,
    aff_id: Option<AfflictionId>,
    fast_energy_hook: FastEnergyHook,
) -> i32 {
    let cost = match fast_energy_hook {
        FastEnergyHook::None => local_e,
        FastEnergyHook::BorrowedTime(delta) => (local_e + delta).max(0),
        FastEnergyHook::Veilpiercer => {
            let is_ethereal = kw_base.contains(CardKeywords::ETHEREAL)
                || (has_hex_power && aff_id == Some(AfflictionId::Hexed));
            if is_ethereal { 0 } else { local_e }
        }
        FastEnergyHook::Corruption | FastEnergyHook::FreeSkill => {
            if card_type == CardType::Skill { 0 } else { local_e }
        }
        FastEnergyHook::FreeAttack => {
            if card_type == CardType::Attack { 0 } else { local_e }
        }
        FastEnergyHook::FreePower => {
            if card_type == CardType::Power { 0 } else { local_e }
        }
        FastEnergyHook::Fallback => unreachable!(),
    };
    #[cfg(debug_assertions)]
    if fast_energy_hook != FastEnergyHook::None {
        let expected = match sts2_kernel::hooks::modify_energy_cost_in_combat_cached_ready(
            state,
            content,
            ch,
            Dec::from_i32(local_e),
            comb,
        ) {
            Ok((m, true)) => m.max(Dec::ZERO).to_i32_trunc().unwrap_or(cost),
            _ => local_e,
        };
        debug_assert_eq!(cost, expected);
    }
    #[cfg(not(debug_assertions))]
    let _ = (state, content, comb, ch);
    cost
}

#[cold]
#[inline(never)]
fn has_living_other_ally(state: &BattleState) -> bool {
    state.ally_handles.iter().any(|&h| {
        h != state.player.creature_handle && state.creature(h).is_some_and(|c| c.is_alive())
    })
}

#[cold]
#[inline(never)]
fn spill_seen_fast_to_keys(
    seen_fast: &[u64; 16],
    seen_fast_len: usize,
    seen_keys: &mut Vec<RolloutDedupKey>,
) {
    seen_keys.clear();
    for &existing in &seen_fast[..seen_fast_len] {
        seen_keys.push(RolloutDedupKey::from_fast_u64(existing));
    }
}

struct ExtractedFastPlayableCard {
    handle: CardHandle,
    card_id: CardId,
    energy_cost: i32,
    star_cost: i32,
    upg: bool,
    exhausts: bool,
    is_retained: bool,
    prof: &'static CardRolloutProfile,
}

#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn try_extract_fast_playable_hand_card(
    state: &mut BattleState,
    content: &dyn ContentDispatcher,
    comb: sts2_types::HookMask,
    fast_energy_hook: FastEnergyHook,
    tables: &'static sts2_kernel::content_dispatch::HookMaskTableView,
    full_profiles: bool,
    cards_contiguous: bool,
    player_energy: i32,
    player_stars: i32,
    has_hex_power: bool,
    bound_blocked: bool,
    ch: CardHandle,
    seen_fast: &mut [u64; 16],
    seen_fast_len: &mut usize,
    use_seen_keys: &mut bool,
    seen_keys: &mut Vec<RolloutDedupKey>,
) -> Option<ExtractedFastPlayableCard> {
    let card_idx = if cards_contiguous {
        let idx = ch.0.wrapping_sub(1) as usize;
        if idx < state.cards.len() {
            #[cfg(debug_assertions)]
            debug_assert_eq!(state.cards[idx].handle, ch);
            idx
        } else {
            return None;
        }
    } else {
        state.card_index(ch)?
    };
    let (
        card_id,
        upgrade_level,
        is_x_e,
        local_e,
        local_s,
        has_ext,
        aff_id,
        kw_base,
        temp_flags,
        target_type,
        has_custom_play,
        prof,
    ) = {
        let card = unsafe { state.cards.get_unchecked(card_idx) };
        if card.pile != PileType::Hand {
            return None;
        }
        let kw_base = if card.keywords_added_from_outside.is_empty()
            && card.keywords_removed_from_outside.is_empty()
        {
            card.local_keywords
        } else {
            card.effective_keywords_base()
        };
        if kw_base.contains(CardKeywords::UNPLAYABLE) {
            return None;
        }
        let card_id = card.id;
        let prof: &'static CardRolloutProfile = if full_profiles {
            unsafe { tables.card_rollout_profiles.get_unchecked(card_id as usize) }
        } else {
            tables.card_rollout_profiles.get(card_id as usize)?
        };
        let mflags = prof.meta_flags;
        let is_x_s = (mflags & mf::IS_X_STAR_COST) != 0;
        let canonical_s = prof.canonical_star_cost as i32;
        let local_s = if is_x_s {
            player_stars
        } else if card.temp_star_cost_override_this_turn.is_none() && card.temp_flags.is_empty() {
            canonical_s
        } else {
            resolve_local_star_cost_card(card, canonical_s)
        };
        if player_stars < local_s {
            return None;
        }
        let is_x_e = (mflags & mf::IS_X_ENERGY_COST) != 0;
        let local_e =
            if card.ext_index.is_none() && !is_x_e && card.canonical_base_energy_cost >= 0 {
                (card.base_energy_cost as i32).max(0)
            } else {
                resolve_local_energy_cost_card(state, card, is_x_e)
            };
        (
            card_id,
            card.upgrade_level,
            is_x_e,
            local_e,
            local_s,
            card.ext_index.is_some(),
            card.affliction.map(|a| a.id),
            kw_base,
            card.temp_flags,
            prof.target_type,
            (mflags & mf::HAS_CUSTOM_PLAYABILITY) != 0,
            prof,
        )
    };
    let card_type = prof.card_type;
    let energy_cost = if is_x_e || fast_energy_hook == FastEnergyHook::None {
        local_e
    } else {
        resolve_fast_energy_hook_cost(
            state,
            content,
            comb,
            ch,
            local_e,
            card_type,
            kw_base,
            has_hex_power,
            aff_id,
            fast_energy_hook,
        )
    };
    if player_energy < energy_cost {
        return None;
    }
    if bound_blocked && aff_id == Some(AfflictionId::Bound) {
        #[cfg(debug_assertions)]
        debug_assert!(
            !sts2_kernel::hooks::should_play_unchecked_active(state, content, ch).unwrap_or(true)
        );
        return None;
    }
    let fast_k =
        RolloutDedupKey::try_fast_u64(card_id, upgrade_level, energy_cost, local_s, has_ext, aff_id);
    #[cfg(debug_assertions)]
    if let Some(fk) = fast_k {
        debug_assert_eq!(
            RolloutDedupKey::from_fast_u64(fk),
            RolloutDedupKey::new(card_id, upgrade_level, energy_cost, local_s, has_ext, aff_id)
        );
    }
    if !*use_seen_keys {
        if let Some(fk) = fast_k {
            for idx in 0..*seen_fast_len {
                if unsafe { *seen_fast.get_unchecked(idx) } == fk {
                    return None;
                }
            }
        }
    } else {
        let key =
            RolloutDedupKey::new(card_id, upgrade_level, energy_cost, local_s, has_ext, aff_id);
        if seen_keys.contains(&key) {
            return None;
        }
    }
    if target_type == TargetType::AnyAlly && !has_living_other_ally(state) {
        return None;
    }
    if has_custom_play && !content.card_is_playable(state, ch).unwrap_or(false) {
        return None;
    }
    let exhausts = kw_base.contains(CardKeywords::EXHAUST)
        || temp_flags.contains(sts2_kernel::state::CardTempFlags::EXHAUST_ON_NEXT_PLAY);
    let is_retained = kw_base.contains(CardKeywords::RETAIN)
        || temp_flags.contains(sts2_kernel::state::CardTempFlags::TEMPORARY_RETAIN_TURN);
    let upg = upgrade_level > 0;
    if !*use_seen_keys {
        if let Some(fk) = fast_k {
            if *seen_fast_len < 16 {
                unsafe {
                    *seen_fast.get_unchecked_mut(*seen_fast_len) = fk;
                }
                *seen_fast_len += 1;
                return Some(ExtractedFastPlayableCard {
                    handle: ch,
                    card_id,
                    energy_cost,
                    star_cost: local_s,
                    upg,
                    exhausts,
                    is_retained,
                    prof,
                });
            }
        }
        spill_seen_fast_to_keys(seen_fast, *seen_fast_len, seen_keys);
        *seen_fast_len = 0;
        *use_seen_keys = true;
    }
    let key = RolloutDedupKey::new(card_id, upgrade_level, energy_cost, local_s, has_ext, aff_id);
    if seen_keys.contains(&key) {
        return None;
    }
    seen_keys.push(key);
    Some(ExtractedFastPlayableCard {
        handle: ch,
        card_id,
        energy_cost,
        star_cost: local_s,
        upg,
        exhausts,
        is_retained,
        prof,
    })
}

#[inline(always)]
fn resolve_fast_scan_meta(
    state: &BattleState,
    comb: sts2_types::HookMask,
    fast_energy_hook: FastEnergyHook,
    fast_should_play: FastShouldPlayHook,
) -> (
    &'static sts2_kernel::content_dispatch::HookMaskTableView,
    bool,
    bool,
    bool,
    bool,
) {
    let tables = state
        .hook_cache
        .cached_tables
        .expect("cache_ready implies cached_tables");
    let full_profiles = CardId::COUNT <= tables.card_rollout_profiles.len();
    let cards_contiguous = state.handle_layout.cards;
    let has_hex_power = fast_energy_hook == FastEnergyHook::Veilpiercer
        && comb.contains(HookSlot::TryModifyKeywordsInCombat)
        && state.player.is_active_for_hooks
        && state.player_creature().is_some_and(|c| {
            !c.is_detached
                && state.contains_creature(c.handle)
                && c.powers.iter().any(|p| p.id == PowerId::HexPower)
        });
    let bound_blocked = matches!(
        fast_should_play,
        FastShouldPlayHook::ChainsOfBinding { bound_played: true }
    );
    (
        tables,
        full_profiles,
        cards_contiguous,
        has_hex_power,
        bound_blocked,
    )
}

fn collect_fast_playable_hand_cards(
    state: &mut BattleState,
    content: &dyn ContentDispatcher,
    comb: sts2_types::HookMask,
    fast_energy_hook: FastEnergyHook,
    fast_should_play: FastShouldPlayHook,
    hand_buf: &mut Vec<CardHandle>,
    seen_keys: &mut Vec<RolloutDedupKey>,
    playable_buf: &mut Vec<PlayableRolloutCard>,
) {
    let (tables, full_profiles, cards_contiguous, has_hex_power, bound_blocked) =
        resolve_fast_scan_meta(state, comb, fast_energy_hook, fast_should_play);
    hand_buf.clear();
    hand_buf.extend_from_slice(&state.player.hand);
    let player_energy = state.player.energy;
    let player_stars = state.player.stars;
    let mut seen_fast = [0u64; 16];
    let mut seen_fast_len = 0usize;
    let mut use_seen_keys = false;
    for i in 0..hand_buf.len() {
        let ch = hand_buf[i];
        #[cfg(debug_assertions)]
        if fast_should_play != FastShouldPlayHook::None {
            let _ = sts2_kernel::hooks::should_play_unchecked_active(state, content, ch);
        }
        if let Some(c) = try_extract_fast_playable_hand_card(
            state,
            content,
            comb,
            fast_energy_hook,
            tables,
            full_profiles,
            cards_contiguous,
            player_energy,
            player_stars,
            has_hex_power,
            bound_blocked,
            ch,
            &mut seen_fast,
            &mut seen_fast_len,
            &mut use_seen_keys,
            seen_keys,
        ) {
            playable_buf.push(PlayableRolloutCard {
                handle: c.handle,
                card_id: c.card_id,
                energy_cost: c.energy_cost,
                star_cost: c.star_cost,
                upg: c.upg,
                exhausts: c.exhausts,
                is_retained: c.is_retained,
            });
        }
    }
}

fn collect_rollout_playable_cards(
    state: &mut BattleState,
    content: &dyn ContentDispatcher,
    hand_buf: &mut Vec<CardHandle>,
    seen_keys: &mut Vec<RolloutDedupKey>,
    playable_buf: &mut Vec<PlayableRolloutCard>,
) {
    playable_buf.clear();
    if state.player.hand.is_empty() || !state.is_in_progress {
        return;
    }
    if !state.hook_cache.masks_are_valid() {
        let _ = state.ensure_hook_cache(content);
    }
    if sts2_kernel::hooks::is_over_or_ending_checked(state, content).unwrap_or(true) {
        return;
    }

    let (fast_query_safe, comb, fast_energy_hook, fast_should_play) =
        resolve_fast_query_and_hooks(state, content);

    if fast_query_safe {
        let tables = state
            .hook_cache
            .cached_tables
            .expect("cache_ready implies cached_tables");
        let full_profiles = CardId::COUNT <= tables.card_rollout_profiles.len();

        if fast_energy_hook != FastEnergyHook::Fallback {
            collect_fast_playable_hand_cards(
                state,
                content,
                comb,
                fast_energy_hook,
                fast_should_play,
                hand_buf,
                seen_keys,
                playable_buf,
            );
            return;
        }
        seen_keys.clear();

        if state.hook_cache.dirty_mask != 0 {
            let _ = state.ensure_hook_cache(content);
        }
        let bound_blocked = matches!(
            fast_should_play,
            FastShouldPlayHook::ChainsOfBinding { bound_played: true }
        );
        let hand_len = state.player.hand.len();
        let mut seen_fast = [0u64; 16];
        let mut seen_fast_len = 0usize;
        for i in 0..hand_len {
            let ch = state.player.hand[i];
            let Some(card_idx) = state.card_index(ch) else {
                continue;
            };
            let (
                card_id,
                upgrade_level,
                is_x_e,
                local_e,
                local_s,
                has_ext,
                aff_id,
                target_type,
                has_custom_play,
                exhausts,
                is_retained,
            ) = {
                let card = &state.cards[card_idx];
                if card.pile != PileType::Hand {
                    continue;
                }
                let kw_base = if card.keywords_added_from_outside.is_empty()
                    && card.keywords_removed_from_outside.is_empty()
                {
                    card.local_keywords
                } else {
                    card.effective_keywords_base()
                };
                if kw_base.contains(CardKeywords::UNPLAYABLE) {
                    continue;
                }
                let card_id = card.id;
                let prof = if full_profiles {
                    unsafe { tables.card_rollout_profiles.get_unchecked(card_id as usize) }
                } else if let Some(p) = tables.card_rollout_profiles.get(card_id as usize) {
                    p
                } else {
                    continue;
                };
                let mflags = prof.meta_flags;
                let is_x_e = (mflags & mf::IS_X_ENERGY_COST) != 0;
                let local_e =
                    if card.ext_index.is_none() && !is_x_e && card.canonical_base_energy_cost >= 0 {
                        (card.base_energy_cost as i32).max(0)
                    } else {
                        resolve_local_energy_cost_card(state, card, is_x_e)
                    };
                let is_x_s = (mflags & mf::IS_X_STAR_COST) != 0;
                let canonical_s = prof.canonical_star_cost as i32;
                let local_s = if is_x_s {
                    state.player.stars
                } else if card.temp_star_cost_override_this_turn.is_none()
                    && card.temp_flags.is_empty()
                {
                    canonical_s
                } else {
                    resolve_local_star_cost_card(card, canonical_s)
                };
                let exhausts = kw_base.contains(CardKeywords::EXHAUST)
                    || card
                        .temp_flags
                        .contains(sts2_kernel::state::CardTempFlags::EXHAUST_ON_NEXT_PLAY);
                let is_retained = kw_base.contains(CardKeywords::RETAIN)
                    || card
                        .temp_flags
                        .contains(sts2_kernel::state::CardTempFlags::TEMPORARY_RETAIN_TURN);
                (
                    card_id,
                    card.upgrade_level,
                    is_x_e,
                    local_e,
                    local_s,
                    card.ext_index.is_some(),
                    card.affliction.map(|a| a.id),
                    prof.target_type,
                    (mflags & mf::HAS_CUSTOM_PLAYABILITY) != 0,
                    exhausts,
                    is_retained,
                )
            };
            if state.player.stars < local_s {
                continue;
            }
            let energy_cost = if is_x_e {
                local_e
            } else {
                match sts2_kernel::hooks::modify_energy_cost_in_combat_cached_ready(
                    state,
                    content,
                    ch,
                    Dec::from_i32(local_e),
                    comb,
                ) {
                    Ok((modified, true)) => {
                        let Ok(v) = modified.max(Dec::ZERO).to_i32_trunc() else {
                            continue;
                        };
                        v
                    }
                    Ok((_, false)) => local_e,
                    Err(_) => continue,
                }
            };
            if state.player.energy < energy_cost {
                continue;
            }
            if bound_blocked && aff_id == Some(AfflictionId::Bound) {
                continue;
            }
            let fast_k = RolloutDedupKey::try_fast_u64(
                card_id,
                upgrade_level,
                energy_cost,
                local_s,
                has_ext,
                aff_id,
            );
            if !seen_keys.is_empty() {
                let key = RolloutDedupKey::new(
                    card_id,
                    upgrade_level,
                    energy_cost,
                    local_s,
                    has_ext,
                    aff_id,
                );
                if seen_keys.contains(&key) {
                    continue;
                }
            } else if let Some(fk) = fast_k {
                if seen_fast[..seen_fast_len].contains(&fk) {
                    continue;
                }
            }
            if target_type == TargetType::AnyAlly {
                let has_ally = state.ally_handles.iter().any(|&h| {
                    h != state.player.creature_handle
                        && state.creature(h).is_some_and(|c| c.is_alive())
                });
                if !has_ally {
                    continue;
                }
            }
            if !has_custom_play
                || content.card_is_playable(state, ch).unwrap_or(false)
            {
                let entry = PlayableRolloutCard {
                    handle: ch,
                    card_id,
                    energy_cost,
                    star_cost: local_s,
                    upg: upgrade_level > 0,
                    exhausts,
                    is_retained,
                };
                if seen_keys.is_empty() {
                    if let Some(fk) = fast_k {
                        if seen_fast_len < 16 {
                            seen_fast[seen_fast_len] = fk;
                            seen_fast_len += 1;
                            playable_buf.push(entry);
                            continue;
                        }
                    }
                    for &existing in &seen_fast[..seen_fast_len] {
                        seen_keys.push(RolloutDedupKey::from_fast_u64(existing));
                    }
                    seen_fast_len = 0;
                }
                let key = RolloutDedupKey::new(
                    card_id,
                    upgrade_level,
                    energy_cost,
                    local_s,
                    has_ext,
                    aff_id,
                );
                if !seen_keys.contains(&key) {
                    seen_keys.push(key);
                    playable_buf.push(entry);
                }
            }
        }
        return;
    }

    seen_keys.clear();
    let mut seen_fast = [0u64; 16];
    let mut seen_fast_len = 0usize;
    hand_buf.clear();
    hand_buf.extend_from_slice(&state.player.hand);
    for i in 0..hand_buf.len() {
        let ch = hand_buf[i];
        let Some(card_idx) = state.card_index(ch) else {
            continue;
        };
        let card = state.cards[card_idx];
        if card.pile != PileType::Hand {
            continue;
        }
        let kw = card.effective_keywords_base();
        if kw.contains(CardKeywords::UNPLAYABLE) {
            continue;
        }
        let Ok(Some((energy_cost, star_cost))) =
            CombatEngine::can_play_card_with_costs_in_progress_at(state, content, ch, card_idx)
        else {
            continue;
        };
        let exhausts = kw.contains(CardKeywords::EXHAUST)
            || card
                .temp_flags
                .contains(sts2_kernel::state::CardTempFlags::EXHAUST_ON_NEXT_PLAY);
        let is_retained = kw.contains(CardKeywords::RETAIN)
            || card
                .temp_flags
                .contains(sts2_kernel::state::CardTempFlags::TEMPORARY_RETAIN_TURN);
        let entry = PlayableRolloutCard {
            handle: ch,
            card_id: card.id,
            energy_cost,
            star_cost,
            upg: card.upgrade_level > 0,
            exhausts,
            is_retained,
        };
        let has_ext = card.ext_index.is_some();
        let aff_id = card.affliction.map(|a| a.id);
        let fast_k = RolloutDedupKey::try_fast_u64(
            card.id,
            card.upgrade_level,
            energy_cost,
            star_cost,
            has_ext,
            aff_id,
        );
        if seen_keys.is_empty() {
            if let Some(fk) = fast_k {
                let mut dup = false;
                for &existing in &seen_fast[..seen_fast_len] {
                    if existing == fk {
                        dup = true;
                        break;
                    }
                }
                if dup {
                    continue;
                }
                if seen_fast_len < 16 {
                    seen_fast[seen_fast_len] = fk;
                    seen_fast_len += 1;
                    playable_buf.push(entry);
                    continue;
                }
            }
            for &existing in &seen_fast[..seen_fast_len] {
                seen_keys.push(RolloutDedupKey::from_fast_u64(existing));
            }
            seen_fast_len = 0;
        }
        let key = RolloutDedupKey::new(
            card.id,
            card.upgrade_level,
            energy_cost,
            star_cost,
            has_ext,
            aff_id,
        );
        if seen_keys.contains(&key) {
            continue;
        }
        seen_keys.push(key);
        playable_buf.push(entry);
    }
}

#[inline]
fn contains_ascii_ignore_case(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let hb = haystack.as_bytes();
    let nb = needle.as_bytes();
    if nb.len() > hb.len() {
        return false;
    }
    hb.windows(nb.len())
        .any(|w| w.eq_ignore_ascii_case(nb))
}

#[inline]
fn is_destructive_hand_choice(id: CardId) -> bool {
    matches!(
        id,
        CardId::Acrobatics
            | CardId::DaggerThrow
            | CardId::HiddenDaggers
            | CardId::Prepared
            | CardId::Survivor
            | CardId::Brand
            | CardId::BurningPact
            | CardId::TrueGrit
            | CardId::Scavenge
            | CardId::Purity
            | CardId::Begone
            | CardId::Guards
            | CardId::Florescence
            | CardId::TheHermit
            | CardId::SculptingStrike
            | CardId::ThinkingAhead
            | CardId::Glimmer
            | CardId::PhotonCut
    )
}

#[inline]
fn should_prefer_highest_hand_choice(
    req: &sts2_kernel::continuation::ChoiceRequest,
    source_card_id: Option<CardId>,
    state: &BattleState,
    content: &dyn ContentDispatcher,
) -> (bool, bool) {
    if let Some(id) = source_card_id {
        match id {
            CardId::Armaments => return (true, true),
            CardId::Nightmare
            | CardId::DualWield
            | CardId::DecisionsDecisions
            | CardId::HeirloomHammer
            | CardId::Transfigure
            | CardId::HandTrick
            | CardId::Snap => return (true, false),
            _ => {}
        }
        if is_destructive_hand_choice(id) {
            return (false, false);
        }
    }

    let sid = req.source_id.as_ref();
    if !sid.is_empty() {
        if contains_ascii_ignore_case(sid, "UPGRADE")
            || contains_ascii_ignore_case(sid, "ARMAMENT")
            || contains_ascii_ignore_case(sid, "APOTHEOSIS")
            || contains_ascii_ignore_case(sid, "SMITH")
            || contains_ascii_ignore_case(sid, "TEMPER")
        {
            return (true, true);
        }
        if contains_ascii_ignore_case(sid, "RETAIN")
            || contains_ascii_ignore_case(sid, "SETUP")
            || contains_ascii_ignore_case(sid, "FORETHOUGHT")
            || contains_ascii_ignore_case(sid, "COPY")
            || contains_ascii_ignore_case(sid, "DUPLICATE")
            || contains_ascii_ignore_case(sid, "CLONE")
            || contains_ascii_ignore_case(sid, "FORGE")
            || contains_ascii_ignore_case(sid, "ENLIGHTEN")
            || contains_ascii_ignore_case(sid, "DISCOUNT")
            || contains_ascii_ignore_case(sid, "REDUCE")
            || contains_ascii_ignore_case(sid, "MIRROR")
            || contains_ascii_ignore_case(sid, "REPEAT")
        {
            return (true, false);
        }
    }

    if req.min_count > 0 {
        if let Some(id) = source_card_id {
            let prof = state.fast_card_rollout_profile(content, id);
            if prof.card_type == CardType::Power || prof.setup_weight > 0 {
                return (true, false);
            }
        }
    }

    (false, false)
}

#[inline]
const fn upgrade_cost_reduction(id: CardId, base_cost: i32) -> i32 {
    if base_cost <= 0 {
        return 0;
    }
    match id {
        CardId::BansheesCry => {
            if base_cost >= 2 {
                2
            } else {
                base_cost
            }
        }
        CardId::Abundance
        | CardId::Alchemize
        | CardId::Apotheosis
        | CardId::Automation
        | CardId::Calamity
        | CardId::Distraction
        | CardId::Entrench
        | CardId::Mayhem
        | CardId::MindBlast
        | CardId::Nostalgia
        | CardId::SovereignBlade
        | CardId::Stratagem
        | CardId::BladeSymphony
        | CardId::BulletTime
        | CardId::Flanking
        | CardId::MasterPlanner
        | CardId::Murder
        | CardId::Nightmare
        | CardId::ShadowStep
        | CardId::Shadowmeld
        | CardId::ToolsOfTheTrade
        | CardId::Tracking
        | CardId::WellLaidPlans
        | CardId::CreativeAi
        | CardId::DoubleEnergy
        | CardId::Dualcast
        | CardId::Feral
        | CardId::Quadcast
        | CardId::SignalBoost
        | CardId::Subroutine
        | CardId::TrashToTreasure
        | CardId::WhiteNoise
        | CardId::Zap
        | CardId::Barricade
        | CardId::BodySlam
        | CardId::Corruption
        | CardId::DarkEmbrace
        | CardId::Havoc
        | CardId::Hellraiser
        | CardId::InfernalBlade
        | CardId::Stampede
        | CardId::Tank
        | CardId::Unmovable
        | CardId::BlotOutTheSky
        | CardId::Ecstasy
        | CardId::Honing
        | CardId::LockedIn
        | CardId::LostLute
        | CardId::Motif
        | CardId::Demesne
        | CardId::Eidolon
        | CardId::ForbiddenGrimoire
        | CardId::Pagestorm
        | CardId::Protector
        | CardId::Sacrifice
        | CardId::Seance
        | CardId::SentryMode
        | CardId::HammerTime
        | CardId::MonarchsGaze
        | CardId::Orbit
        | CardId::SevenStars
        | CardId::SpectrumShift
        | CardId::SwordSage
        | CardId::TheSealedThrone
        | CardId::Tutor => 1,
        _ => 0,
    }
}

fn compute_generic_choice_card_quality(
    state: &BattleState,
    content: &dyn ContentDispatcher,
    ch: CardHandle,
) -> i32 {
    let Some(card) = state.card(ch) else {
        return 0;
    };
    let Ok(meta) = content.card_meta(card.id) else {
        return 0;
    };
    if card.has_local_keyword(CardKeywords::UNPLAYABLE) {
        return -450;
    }
    if matches!(meta.card_type, CardType::Curse | CardType::Status) {
        return -400;
    }
    let mut quality = if meta
        .base_tags
        .intersects(CardTags::STRIKE | CardTags::DEFEND)
    {
        60
    } else {
        let mut q = match meta.card_type {
            CardType::Power => 280,
            CardType::Skill => 200,
            CardType::Attack => 190,
            _ => 120,
        };
        q += match meta.rarity {
            CardRarity::Rare | CardRarity::Ancient => 90,
            CardRarity::Uncommon => 50,
            CardRarity::Common | CardRarity::Event => 20,
            _ => 10,
        };
        let setup_weight = state.fast_card_rollout_profile(content, card.id).setup_weight as i32;
        q += (setup_weight * 4).min(140);
        let base_cost = card.base_energy_cost;
        if base_cost == 0 {
            q += 40;
        } else if base_cost == 1 {
            q += 15;
        }
        q
    };
    if card.upgrade_level > 0 {
        quality += 35;
    }
    if !meta.flags.contains(CardStaticFlags::IS_X_ENERGY_COST)
        && sts2_kernel::ordering::resolve_local_energy_cost_card(state, card, false) == 0
    {
        quality += 30;
    }
    quality
}

#[inline(always)]
fn fast_generic_choice_card_quality(
    state: &BattleState,
    content: &dyn ContentDispatcher,
    cards: &[sts2_kernel::state::CardInst],
    cards_contiguous: bool,
    qtable: Option<&[GenericChoiceQualityEntry]>,
    root_choice_q: &[i16],
    ch: CardHandle,
) -> i32 {
    if cards_contiguous && !root_choice_q.is_empty() {
        let idx = ch.0.wrapping_sub(1) as usize;
        if idx < root_choice_q.len() && !state.is_card_meta_dirty(idx) {
            let q = unsafe { *root_choice_q.get_unchecked(idx) };
            if q != i16::MIN {
                #[cfg(debug_assertions)]
                debug_assert_eq!(
                    q as i32,
                    compute_generic_choice_card_quality(state, content, ch)
                );
                return q as i32;
            }
        }
    }
    if let Some(qt) = qtable {
        let idx = ch.0.wrapping_sub(1) as usize;
        let card = if cards_contiguous {
            if idx < cards.len() {
                let c = unsafe { cards.get_unchecked(idx) };
                #[cfg(debug_assertions)]
                debug_assert_eq!(c.handle, ch);
                Some(c)
            } else {
                None
            }
        } else if idx < cards.len() && cards[idx].handle == ch {
            Some(&cards[idx])
        } else {
            state.card(ch)
        };
        let Some(card) = card else {
            return 0;
        };
        if card.has_local_keyword(CardKeywords::UNPLAYABLE) {
            return -450;
        }
        let cid_idx = card.id as usize;
        if cid_idx < qt.len() {
            let e = unsafe { qt.get_unchecked(cid_idx) };
            if (e.flags & 1) != 0 {
                return -400;
            }
            let quality = if card.ext_index.is_none()
                && card.canonical_base_energy_cost >= 0
                && card.base_energy_cost >= 0
            {
                let base_q = match card.base_energy_cost {
                    0 => e.q_cost0,
                    1 => e.q_cost1,
                    _ => e.q_cost2p,
                } as i32;
                base_q + if card.upgrade_level > 0 {
                    e.upg_bonus as i32
                } else {
                    0
                }
            } else {
                let mut q = e.q_cost2p as i32;
                if (e.flags & 2) == 0 {
                    let base_cost = card.base_energy_cost;
                    if base_cost == 0 {
                        q += 40;
                    } else if base_cost == 1 {
                        q += 15;
                    }
                }
                if card.upgrade_level > 0 {
                    q += 35;
                }
                if (e.flags & 4) == 0 {
                    let cost_is_zero =
                        if card.ext_index.is_none() && card.canonical_base_energy_cost >= 0 {
                            card.base_energy_cost <= 0
                        } else {
                            resolve_local_energy_cost_card(state, card, false) == 0
                        };
                    if cost_is_zero {
                        q += 30;
                    }
                }
                q
            };
            #[cfg(debug_assertions)]
            debug_assert_eq!(
                quality,
                compute_generic_choice_card_quality(state, content, ch)
            );
            return quality;
        }
    }
    compute_generic_choice_card_quality(state, content, ch)
}

#[inline]
fn resolve_card_choice_energy_cost_ro(
    state: &BattleState,
    card: &sts2_kernel::state::CardInst,
    card_type: CardType,
    is_costs_x: bool,
    local_cost: i32,
) -> Option<i32> {
    if is_costs_x {
        return Some(0);
    }
    if !state.hook_cache.masks_are_valid() {
        return None;
    }
    let comb = state.hook_cache.masks.combat_combined;
    let has_early = comb.contains(HookSlot::TryModifyEnergyCostInCombat);
    let has_late = comb.contains(HookSlot::TryModifyEnergyCostInCombatLate);
    if !has_early && !has_late {
        return Some(local_cost);
    }
    if has_early && !has_late {
        let pw = resolve_sole_player_power_in_slot(state, HookSlot::TryModifyEnergyCostInCombat)?;
        if pw.id == PowerId::BorrowedTimePower {
            return Some((local_cost + pw.amount).max(0));
        }
        return None;
    }
    if !has_early && has_late {
        let pw =
            resolve_sole_player_power_in_slot(state, HookSlot::TryModifyEnergyCostInCombatLate)?;
        return match pw.id {
            PowerId::CorruptionPower | PowerId::FreeSkillPower => {
                Some(if card_type == CardType::Skill { 0 } else { local_cost })
            }
            PowerId::FreeAttackPower => {
                Some(if card_type == CardType::Attack { 0 } else { local_cost })
            }
            PowerId::FreePowerPower => {
                Some(if card_type == CardType::Power { 0 } else { local_cost })
            }
            PowerId::VeilpiercerPower => {
                let is_ethereal = card.has_local_keyword(CardKeywords::ETHEREAL)
                    || (comb.contains(HookSlot::TryModifyKeywordsInCombat)
                        && card.affliction.is_some_and(|a| a.id == AfflictionId::Hexed)
                        && state
                            .player_creature()
                            .is_some_and(|pc| pc.powers.iter().any(|p| p.id == PowerId::HexPower)));
                Some(if is_ethereal { 0 } else { local_cost })
            }
            _ => None,
        };
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn compute_choice_card_instance_quality_ro(
    state: &BattleState,
    content: &dyn ContentDispatcher,
    cards: &[sts2_kernel::state::CardInst],
    cards_contiguous: bool,
    qtable: Option<&[GenericChoiceQualityEntry]>,
    root_choice_q: &[i16],
    ch: CardHandle,
    unblocked_incoming: i32,
    available_energy: i32,
    is_upgrade_prompt: bool,
    has_retain_hand: bool,
    eff_cost_override: Option<i32>,
) -> i32 {
    let Some(card) = state.card(ch) else {
        return 0;
    };
    let Ok(meta) = content.card_meta(card.id) else {
        return 0;
    };
    if card.has_local_keyword(CardKeywords::UNPLAYABLE) {
        return -450;
    }
    if matches!(meta.card_type, CardType::Curse | CardType::Status) {
        return -400;
    }

    let prof = state.fast_card_rollout_profile(content, card.id);
    let is_costs_x = meta.flags.contains(CardStaticFlags::IS_X_ENERGY_COST);
    let local_cost = if is_costs_x {
        0
    } else {
        resolve_local_energy_cost_card(state, card, false).max(0)
    };
    let eff_cost = eff_cost_override
        .or_else(|| {
            resolve_card_choice_energy_cost_ro(state, card, meta.card_type, is_costs_x, local_cost)
        })
        .unwrap_or(local_cost);

    let mut quality = if !is_upgrade_prompt {
        let mut q = fast_generic_choice_card_quality(
            state,
            content,
            cards,
            cards_contiguous,
            qtable,
            root_choice_q,
            ch,
        );
        if !is_costs_x && local_cost != 0 && eff_cost == 0 {
            q += 30;
        }
        q
    } else {
        let mut q = if meta
            .base_tags
            .intersects(CardTags::STRIKE | CardTags::DEFEND)
        {
            60
        } else {
            let mut bq = match meta.card_type {
                CardType::Power => 280,
                CardType::Skill => 200,
                CardType::Attack => 190,
                _ => 120,
            };
            bq += match meta.rarity {
                CardRarity::Rare | CardRarity::Ancient => 90,
                CardRarity::Uncommon => 50,
                CardRarity::Common | CardRarity::Event => 20,
                _ => 10,
            };
            bq += ((prof.setup_weight as i32) * 4).min(140);
            if card.base_energy_cost == 0 {
                bq += 40;
            } else if card.base_energy_cost == 1 {
                bq += 15;
            }
            bq
        };
        if card.upgrade_level >= meta.max_upgrade_level {
            q -= 300;
        } else {
            let cost_red = upgrade_cost_reduction(card.id, card.base_energy_cost as i32);
            if cost_red > 0 {
                q += cost_red * 70;
            }
            let base_total_dmg = (prof.base_damage as i32) * (prof.hits.max(1) as i32);
            let upg_total_dmg = (prof.upg_damage as i32) * (prof.upg_hits.max(1) as i32);
            let dmg_gain = (upg_total_dmg - base_total_dmg).max(0);
            let blk_gain = ((prof.upg_block as i32) - (prof.base_block as i32)).max(0);
            q += (dmg_gain * 4 + blk_gain * 5).min(90);
        }
        if !is_costs_x && (eff_cost == 0 || local_cost == 0) {
            q += 30;
        }
        q
    };

    if card.id == CardId::SovereignBlade {
        let dmg = read_comparison_var(state, content, ch, "Damage").unwrap_or(10);
        let forged_delta = (dmg - 10).max(0);
        if forged_delta > 0 {
            quality += (forged_delta * 8).min(240);
        }
    }

    let affordable_now = available_energy < 0 || eff_cost <= available_energy;
    let gains_block = meta.flags.contains(CardStaticFlags::GAINS_BLOCK)
        || meta.base_tags.contains(CardTags::DEFEND);
    let player_hp = state
        .player_creature()
        .map_or(state.player.current_hp, |c| c.current_hp);

    if unblocked_incoming > 0 && gains_block && affordable_now {
        let mut raw_block = if card.id == CardId::GeneticAlgorithm {
            read_genetic_algorithm_block(state, content, ch, card.upgrade_level > 0, &prof)
        } else if card.upgrade_level > 0 {
            prof.upg_block as i32
        } else {
            prof.base_block as i32
        };
        if raw_block <= 0 {
            raw_block = 5;
        }
        let useful_block = raw_block.min(unblocked_incoming);
        let lethal = unblocked_incoming >= player_hp;
        let heavy_threat = lethal
            || unblocked_incoming >= 12
            || (unblocked_incoming * 4 >= player_hp.max(1));
        if lethal {
            quality += 220 + (useful_block * 14).min(180);
        } else if heavy_threat {
            quality += 170 + (useful_block * 10).min(130);
        } else {
            quality += 70 + (useful_block * 8).min(80);
        }
    }

    let is_retained = has_retain_hand
        || card.has_local_keyword(CardKeywords::RETAIN)
        || card.temp_flags.contains(CardTempFlags::TEMPORARY_RETAIN_TURN);
    let has_draw =
        (prof.op_flags & op::HAS_DRAW) != 0 || card.id == CardId::CosmicIndifference;

    if available_energy >= 0 && eff_cost > available_energy && !is_retained && !has_draw {
        let pre_penalty = quality;
        let urgent_threat = unblocked_incoming > 0
            && (unblocked_incoming >= player_hp
                || unblocked_incoming >= 12
                || (unblocked_incoming * 4 >= player_hp.max(1)));
        quality -= if urgent_threat { 25 } else { 15 };
        let floor = if !is_upgrade_prompt && pre_penalty > 80 {
            85
        } else {
            10
        };
        if !is_upgrade_prompt && quality < floor {
            quality = floor;
        }
    }

    quality
}

fn should_hold_or_skip_copy_card(
    state: &BattleState,
    content: &dyn ContentDispatcher,
    copy_card_handle: CardHandle,
    attack_or_power_only: bool,
    can_retain: bool,
    lethal_threat: bool,
) -> bool {
    let qtable = state
        .hook_cache
        .cached_tables
        .map(|t| t.card_generic_choice_quality);
    let cards = &state.cards;
    let cards_contiguous = state.handle_layout.cards;

    let mut best_hand_quality = i32::MIN;
    let mut best_hand_setup = 0i32;

    for &h_ch in &state.player.hand {
        if h_ch == copy_card_handle {
            continue;
        }
        let Some(c) = state.card(h_ch) else {
            continue;
        };
        let prof = state.fast_card_rollout_profile(content, c.id);
        let resolved_type = if c.id == CardId::MadScience {
            content.card_resolved_type(state, h_ch).unwrap_or(prof.card_type)
        } else {
            prof.card_type
        };
        if attack_or_power_only && !matches!(resolved_type, CardType::Attack | CardType::Power) {
            continue;
        }
        let q = compute_choice_card_instance_quality_ro(
            state,
            content,
            cards,
            cards_contiguous,
            qtable,
            &[],
            h_ch,
            0,
            -1,
            false,
            false,
            None,
        );
        let sw = prof.setup_weight as i32;
        if q > best_hand_quality {
            best_hand_quality = q;
        }
        if sw > best_hand_setup {
            best_hand_setup = sw;
        }
    }

    if best_hand_quality == i32::MIN {
        return true;
    }
    if !can_retain || lethal_threat || state.player.draw.is_empty() {
        return false;
    }

    let mut best_draw_quality = i32::MIN;
    let mut best_draw_setup = 0i32;
    for &d_ch in &state.player.draw {
        let Some(c) = state.card(d_ch) else {
            continue;
        };
        let prof = state.fast_card_rollout_profile(content, c.id);
        let resolved_type = if c.id == CardId::MadScience {
            content.card_resolved_type(state, d_ch).unwrap_or(prof.card_type)
        } else {
            prof.card_type
        };
        if attack_or_power_only && !matches!(resolved_type, CardType::Attack | CardType::Power) {
            continue;
        }
        let q = compute_choice_card_instance_quality_ro(
            state,
            content,
            cards,
            cards_contiguous,
            qtable,
            &[],
            d_ch,
            0,
            -1,
            false,
            false,
            None,
        );
        let sw = prof.setup_weight as i32;
        if q > best_draw_quality {
            best_draw_quality = q;
        }
        if sw > best_draw_setup {
            best_draw_setup = sw;
        }
    }

    if best_draw_quality == i32::MIN {
        return false;
    }
    if best_hand_quality < 0 && best_draw_quality >= 0 {
        return true;
    }
    if best_hand_setup < 18 && best_draw_setup >= 18 {
        return true;
    }
    if best_draw_setup >= best_hand_setup + 15 && best_draw_setup >= 24 {
        return true;
    }
    best_hand_quality < 280 && best_draw_quality >= best_hand_quality + 120
}

#[allow(clippy::too_many_arguments)]
fn pick_rollout_choice(
    state: &mut BattleState,
    content: &dyn ContentDispatcher,
    root_choice_q: &[i16],
    req: &sts2_kernel::continuation::ChoiceRequest,
    unblocked_incoming: i32,
    has_retain_hand: bool,
    scored: &mut Vec<(usize, i32, u32)>,
    sel_out: &mut SmallVec<[usize; 16]>,
) -> bool {
    sel_out.clear();
    let has_none_option = req.min_count == 0 || req.can_skip;
    if req.candidates.is_empty() {
        return has_none_option;
    }

    let source_card_id = if req.source_id.starts_with("CARD.") {
        CardId::from_model_id_str(&req.source_id)
    } else {
        None
    };

    let (is_beneficial_hand_choice, is_upgrade_prompt) = if req.source_pile == PileType::Hand {
        should_prefer_highest_hand_choice(req, source_card_id, state, content)
    } else {
        (false, false)
    };
    let is_destructive_draw_choice = req.source_pile == PileType::Draw
        && matches!(
            source_card_id,
            Some(CardId::Charge | CardId::Cleanse | CardId::Seance)
        );
    let is_guards_hand_choice =
        req.source_pile == PileType::Hand && source_card_id == Some(CardId::Guards);

    let prefer_lowest =
        (req.source_pile == PileType::Hand && !is_beneficial_hand_choice) || is_destructive_draw_choice;
    let is_next_turn_pile_choice = is_destructive_draw_choice
        || (req.source_pile == PileType::Discard
            && matches!(
                source_card_id,
                Some(CardId::CosmicIndifference | CardId::Headbutt | CardId::WahKi)
            ));

    let eval_unblocked_incoming = if is_guards_hand_choice {
        0
    } else {
        unblocked_incoming
    };
    let eval_available_energy = if is_beneficial_hand_choice || is_next_turn_pile_choice {
        -1
    } else {
        state.player.energy
    };

    let need_cost_resolve = eval_available_energy >= 0
        && state.hook_cache.masks_are_valid()
        && (state
            .hook_cache
            .masks
            .combat_combined
            .contains(HookSlot::TryModifyEnergyCostInCombat)
            || state
                .hook_cache
                .masks
                .combat_combined
                .contains(HookSlot::TryModifyEnergyCostInCombatLate));

    scored.clear();
    scored.reserve(req.candidates.len().saturating_sub(scored.capacity()));

    for (idx, &ch) in req.candidates.iter().enumerate() {
        if has_none_option
            && is_guards_hand_choice
            && state.card(ch).is_some_and(|c| c.id == CardId::SovereignBlade)
        {
            continue;
        }
        let eff_cost_override = if need_cost_resolve {
            let meta_is_x = state
                .card(ch)
                .and_then(|c| content.card_meta(c.id).ok())
                .is_some_and(|m| m.flags.contains(CardStaticFlags::IS_X_ENERGY_COST));
            if meta_is_x {
                Some(0)
            } else {
                resolve_energy_cost(state, content, ch).ok().map(|c| c.max(0))
            }
        } else {
            None
        };
        let qtable = state
            .hook_cache
            .cached_tables
            .map(|t| t.card_generic_choice_quality);
        let cards = &state.cards;
        let cards_contiguous = state.handle_layout.cards;
        let q = compute_choice_card_instance_quality_ro(
            state,
            content,
            cards,
            cards_contiguous,
            qtable,
            root_choice_q,
            ch,
            eval_unblocked_incoming,
            eval_available_energy,
            is_upgrade_prompt,
            has_retain_hand,
            eff_cost_override,
        );
        scored.push((idx, q, ch.0));
    }

    if scored.is_empty() {
        return has_none_option;
    }

    let cmp = |a: &(usize, i32, u32), b: &(usize, i32, u32)| {
        let q_cmp = if prefer_lowest {
            a.1.cmp(&b.1)
        } else {
            b.1.cmp(&a.1)
        };
        q_cmp.then_with(|| a.2.cmp(&b.2)).then_with(|| a.0.cmp(&b.0))
    };

    let max_optional_cap = 5usize;
    let optional_pick_limit = if req.source_pile == PileType::None && !is_beneficial_hand_choice {
        1usize.min(req.max_count)
    } else if is_beneficial_hand_choice {
        req.max_count.min(scored.len())
    } else {
        let base_cap = req.max_count.min(max_optional_cap);
        if is_guards_hand_choice {
            base_cap.min(4)
        } else {
            base_cap
        }
    };

    if !has_none_option {
        let take_n = req.min_count.min(scored.len());
        if take_n > 0 {
            if take_n < scored.len() {
                scored.select_nth_unstable_by(take_n - 1, cmp);
                scored[..take_n].sort_unstable_by(cmp);
            } else {
                scored.sort_unstable_by(cmp);
            }
            sel_out.extend(scored[..take_n].iter().map(|x| x.0));
        }
        return sel_out.len() >= req.min_count;
    }

    // Optional prompt (min_count == 0 or can_skip == true)
    let limit = optional_pick_limit.min(scored.len());
    if limit == 0 {
        return true;
    }
    if limit < scored.len() {
        scored.select_nth_unstable_by(limit - 1, cmp);
        scored[..limit].sort_unstable_by(cmp);
    } else {
        scored.sort_unstable_by(cmp);
    }

    if prefer_lowest {
        let player_hp = state
            .player_creature()
            .map_or(state.player.current_hp, |c| c.current_hp);
        let player_max_hp = state
            .player_creature()
            .map_or(state.player.max_hp, |c| c.max_hp);
        let non_lethal_safe = unblocked_incoming == 0
            || (unblocked_incoming < player_hp
                && player_hp - unblocked_incoming > 4.max(player_max_hp / 5));
        let max_allowed_quality = if is_guards_hand_choice {
            if unblocked_incoming > 0 { 80 } else { -1 }
        } else if non_lethal_safe {
            75
        } else if unblocked_incoming < player_hp {
            65
        } else {
            -1
        };
        for &(idx, q, _) in &scored[..limit] {
            if q > max_allowed_quality {
                break;
            }
            sel_out.push(idx);
        }
    } else {
        let is_generated = req.source_pile == PileType::None;
        let hand_nearly_full = state.player.hand.len() >= 9;
        for &(idx, q, _) in &scored[..limit] {
            if q < 0 || (is_generated && hand_nearly_full && q < 250) {
                break;
            }
            sel_out.push(idx);
        }
    }

    if req.can_skip && sel_out.len() < req.min_count {
        sel_out.clear();
    }
    true
}

#[allow(clippy::too_many_arguments)]
fn resolve_step_choices(
    state: &mut BattleState,
    content: &dyn ContentDispatcher,
    root_baseline: &RootEvalBaseline,
    start_turn: i32,
    mut ctx_choice_info: Option<(i32, bool)>,
    mut outcome: StepOutcome,
    choice_slab: &mut RolloutChoiceSlab,
    scored_buf: &mut Vec<(usize, i32, u32)>,
    sel_buf: &mut SmallVec<[usize; 16]>,
) -> Option<(u32, u16)> {
    let meta_start = choice_slab.steps_meta.len();
    let idx_start = choice_slab.indices.len();
    let mut depth = 0usize;
    let mut lazy_intent_cache: Option<IntentHitsCache> = None;
    while let StepOutcome::NeedsChoice(ref req) = outcome {
        let (unblocked_incoming, has_retain_hand) = match ctx_choice_info {
            Some((inc, ret)) => (
                if state.player.turn_number != start_turn {
                    0
                } else {
                    inc
                },
                ret,
            ),
            None => {
                let ic = lazy_intent_cache.get_or_insert_with(IntentHitsCache::new);
                let tmp_ctx = compute_fast_turn_context::<false>(
                    state,
                    content,
                    root_baseline,
                    ic,
                );
                ctx_choice_info = Some((tmp_ctx.unblocked_incoming, tmp_ctx.has_retain_hand));
                (
                    if state.player.turn_number != start_turn {
                        0
                    } else {
                        tmp_ctx.unblocked_incoming
                    },
                    tmp_ctx.has_retain_hand,
                )
            }
        };
        if depth >= 4
            || !pick_rollout_choice(
                state,
                content,
                &root_baseline.root_card_choice_quality,
                req,
                unblocked_incoming,
                has_retain_hand,
                scored_buf,
                sel_buf,
            )
        {
            choice_slab.truncate_to(meta_start, idx_start);
            return None;
        }
        let Ok(next) = CombatEngine::resume_choice(state, content, sel_buf) else {
            choice_slab.truncate_to(meta_start, idx_start);
            return None;
        };
        choice_slab.push_step(sel_buf);
        outcome = next;
        depth += 1;
    }
    Some((
        meta_start as u32,
        (choice_slab.steps_meta.len() - meta_start) as u16,
    ))
}

fn replay_compact_steps(
    state: &mut BattleState,
    content: &dyn ContentDispatcher,
    steps: &[CompactRolloutStep],
    choices: &RolloutChoiceSlab,
    choice_q: &mut SmallVec<[Vec<usize>; 2]>,
    sel_buf: &mut SmallVec<[usize; 16]>,
) -> Result<u16, sts2_kernel::KernelError> {
    let mut auto_potions = 0u16;
    for step in steps {
        let potions_before = count_non_empty_potion_slots(state);
        choice_q.clear();
        if step.is_end_turn == 0 {
            let target = if step.target_handle == u32::MAX {
                None
            } else {
                Some(CreatureHandle(step.target_handle))
            };
            for c_i in 0..step.choices_len as usize {
                let s = choices.step_indices(step.choices_offset as usize + c_i);
                choice_q.push(s.iter().map(|&x| x as usize).collect());
            }
            let mut out = CombatEngine::play_card(
                state,
                content,
                CardHandle(step.card_handle),
                target,
                choice_q,
            )?;
            while matches!(out, StepOutcome::NeedsChoice(_)) && !choice_q.is_empty() {
                let sel = choice_q.remove(0);
                out = CombatEngine::resume_choice(state, content, &sel)?;
            }
            if matches!(out, StepOutcome::NeedsChoice(_)) {
                return Err(sts2_kernel::KernelError::InvalidAction(
                    "compact rollout replay exhausted queued choices".to_owned(),
                ));
            }
        } else {
            let mut out = CombatEngine::end_player_turn(state, content)?;
            let mut c_idx = 0usize;
            while matches!(out, StepOutcome::NeedsChoice(_)) {
                if c_idx >= step.choices_len as usize {
                    return Err(sts2_kernel::KernelError::InvalidAction(
                        "compact rollout end-turn replay missing choice".to_owned(),
                    ));
                }
                let s = choices.step_indices(step.choices_offset as usize + c_idx);
                sel_buf.clear();
                sel_buf.extend(s.iter().map(|&x| x as usize));
                out = CombatEngine::resume_choice(state, content, sel_buf)?;
                c_idx += 1;
            }
        }
        let potions_after = count_non_empty_potion_slots(state);
        auto_potions += potions_before.saturating_sub(potions_after) as u16;
    }
    Ok(auto_potions)
}

#[inline]
fn read_comparison_var(
    state: &BattleState,
    content: &dyn ContentDispatcher,
    ch: CardHandle,
    name: &str,
) -> Option<i32> {
    let vals = content.card_comparison_base_values(state, ch).ok()?;
    for (k, v) in vals {
        if k == name {
            return Some(v.to_i32_trunc().unwrap_or(0).max(0));
        }
    }
    None
}

#[derive(Clone, Copy, Default)]
struct RolloutStrategyContext {
    active: bool,
    pure_growth_unsatisfied: bool,
    relic_unsatisfied: bool,
    unsatisfied_relic_mask: u64,
    relic_distance: i32,
    has_lethal_in_hand: bool,
    hold_last_enemy_lethal: bool,
    has_empty_potion_slot: bool,
    has_any_non_minion_enemy: bool,
    remaining_strategy_budget: i32,
}

#[inline]
fn is_mad_science_improvement(
    state: &BattleState,
    card: &sts2_kernel::state::CardInst,
) -> bool {
    if card.id != CardId::MadScience {
        return false;
    }
    let Some(ext) = state.card_ext(card.ext_index) else {
        return false;
    };
    let is_power = matches!(
        ext.private_fields.get("MadScience._type"),
        Some(FieldValue::String(s)) if s == "Power"
    ) || matches!(
        ext.private_fields.get("MadScience._type"),
        Some(FieldValue::Int(3))
    );
    let is_improvement = matches!(
        ext.private_fields.get("MadScience.<Rider>k__BackingField"),
        Some(FieldValue::String(s)) if s == "Improvement"
    ) || matches!(
        ext.private_fields.get("MadScience.<Rider>k__BackingField"),
        Some(FieldValue::Int(9))
    );
    is_power && is_improvement
}

#[inline]
fn is_goopy_deck_backed(
    state: &BattleState,
    card: &sts2_kernel::state::CardInst,
) -> bool {
    card.deck_card_handle.is_some()
        && state
            .card_ext(card.ext_index)
            .and_then(|e| e.enchantment.as_ref())
            .is_some_and(|en| en.id == EnchantmentId::Goopy)
}

#[inline]
fn has_any_non_minion_living_enemy(state: &BattleState) -> bool {
    for &eh in &state.enemy_handles {
        let Some(ec) = state.creature(eh) else {
            continue;
        };
        if !ec.is_alive() || ec.current_hp <= 0 || ec.is_secondary_enemy() {
            continue;
        }
        if !ec
            .powers
            .iter()
            .any(|p| p.id == PowerId::MinionPower && p.amount > 0)
        {
            return true;
        }
    }
    false
}

#[inline]
fn should_enemy_death_trigger_fatal_fast(state: &BattleState, eh: CreatureHandle) -> bool {
    let Some(ec) = state.creature(eh) else {
        return false;
    };
    if ec.is_secondary_enemy()
        || ec
            .powers
            .iter()
            .any(|p| p.id == PowerId::MinionPower && p.amount > 0)
    {
        return false;
    }
    let has_reattach = ec
        .powers
        .iter()
        .any(|p| p.id == PowerId::ReattachPower && p.amount > 0);
    if has_reattach {
        for &other_h in &state.enemy_handles {
            if other_h == eh {
                continue;
            }
            if state.creature(other_h).is_some_and(|oc| {
                oc.is_alive()
                    && oc
                        .powers
                        .iter()
                        .any(|p| p.id == PowerId::ReattachPower && p.amount > 0)
            }) {
                return false;
            }
        }
    }
    true
}

#[inline]
fn card_advances_unsatisfied_relic(
    card_type: CardType,
    exhausts: bool,
    star_cost: i32,
    unsatisfied_relic_mask: u64,
) -> bool {
    if (unsatisfied_relic_mask & CARD_RELIC_MASK) != 0 {
        return true;
    }
    if (unsatisfied_relic_mask & ATTACK_RELIC_MASK) != 0 && card_type == CardType::Attack {
        return true;
    }
    if (unsatisfied_relic_mask & SKILL_RELIC_MASK) != 0 && card_type == CardType::Skill {
        return true;
    }
    if (unsatisfied_relic_mask & EXHAUST_RELIC_MASK) != 0 && exhausts {
        return true;
    }
    if (unsatisfied_relic_mask & STAR_RELIC_MASK) != 0 && star_cost > 0 {
        return true;
    }
    false
}

#[allow(clippy::too_many_arguments)]
fn get_growth_card_budget(
    state: &BattleState,
    card: &sts2_kernel::state::CardInst,
    root: &RootEvalBaseline,
    has_empty_potion_slot: bool,
    has_any_non_minion_enemy: bool,
    mad_science_count: i32,
) -> (i32, bool, bool) {
    let b = root.effective_growth_budgets;
    let mut is_fatal_finisher = false;
    let mut is_playable_growth = false;
    let mut budget = -1i32;

    match card.id {
        CardId::HandOfGreed => {
            if has_any_non_minion_enemy {
                is_fatal_finisher = true;
                budget = budget.max(b.hand_of_greed);
            }
        }
        CardId::TheHunt => {
            if has_any_non_minion_enemy {
                is_fatal_finisher = true;
                budget = budget.max(b.the_hunt);
            }
        }
        CardId::Feed => {
            if has_any_non_minion_enemy {
                is_fatal_finisher = true;
                budget = budget.max(b.feed);
            }
        }
        CardId::Royalties => {
            is_playable_growth = true;
            budget = budget.max(b.royalties);
        }
        CardId::Alchemize => {
            if !root.has_sozu && has_empty_potion_slot {
                is_playable_growth = true;
                budget = budget.max(b.alchemize);
            }
        }
        CardId::GeneticAlgorithm => {
            if card.deck_card_handle.is_some() {
                is_playable_growth = true;
                budget = budget.max(b.genetic_algorithm);
            }
        }
        CardId::TheScythe => {
            if card.deck_card_handle.is_some() {
                is_playable_growth = true;
                budget = budget.max(b.the_scythe);
            }
        }
        CardId::ForbiddenGrimoire => {
            is_playable_growth = true;
            budget = budget.max(b.forbidden_grimoire);
        }
        CardId::MadScience => {
            if is_mad_science_improvement(state, card)
                && mad_science_count < root.mad_science_upgrade_capacity
            {
                is_playable_growth = true;
                budget = budget.max(b.mad_science);
            }
        }
        CardId::ShowOff => {
            if has_any_non_minion_enemy {
                is_playable_growth = true;
                budget = budget.max(0);
            }
        }
        _ => {}
    }

    if is_goopy_deck_backed(state, card) {
        is_playable_growth = true;
        budget = budget.max(b.goopy);
    }

    (budget, is_fatal_finisher, is_playable_growth)
}

#[inline]
pub(crate) fn resolve_card_instance_base_damage(
    state: &BattleState,
    content: &dyn ContentDispatcher,
    ch: CardHandle,
    card_id: CardId,
    upg: bool,
    profile: &CardRolloutProfile,
) -> i32 {
    let static_dmg = if upg {
        profile.upg_damage as i32
    } else {
        profile.base_damage as i32
    };
    if card_has_dynamic_comparison_values(card_id) {
        return read_comparison_var(state, content, ch, "Damage").unwrap_or(static_dmg);
    }
    if matches!(card_id, CardId::Bolas | CardId::ThrummingHatchet) {
        let key = if card_id == CardId::Bolas {
            "Bolas._damage"
        } else {
            "ThrummingHatchet._damage"
        };
        if let Some(ext) = state.card(ch).and_then(|c| state.card_ext(c.ext_index)) {
            if let Some(fv) = ext.private_fields.get(key) {
                if let FieldValue::Decimal(d) = fv {
                    return d.to_i32_trunc().unwrap_or(static_dmg);
                }
            }
        }
    }
    static_dmg
}

#[inline]
fn estimate_dynamic_card_base_damage(
    state: &BattleState,
    content: &dyn ContentDispatcher,
    ch: CardHandle,
    card_id: CardId,
    upg: bool,
    profile: &CardRolloutProfile,
    ctx: &FastTurnContext,
) -> i32 {
    if card_id == CardId::BodySlam {
        return ctx.player_block;
    }
    resolve_card_instance_base_damage(state, content, ch, card_id, upg, profile)
}

#[allow(clippy::too_many_arguments)]
fn build_rollout_strategy_context<const IS_SETUP: bool>(
    state: &BattleState,
    content: &dyn ContentDispatcher,
    root: &RootEvalBaseline,
    playable: &[PlayableRolloutCard],
    blocked: &[CardHandle],
    ctx: &FastTurnContext,
    lethal_threat: bool,
) -> RolloutStrategyContext {
    let mut strat = RolloutStrategyContext::default();
    let pure_growth_unsatisfied = root.effective_has_growth_targets
        && !root
            .growth_opportunity_targets
            .is_satisfied_by(&state.semantic_history.growth_rewards);
    let (unsatisfied_relic_mask, relic_distance) = if !root.active_relic_targets.is_empty() {
        let re_if_win = RelicCounterPolicy::evaluate(
            state,
            &root.active_relic_targets,
            true,
            root.post_combat_relic_heal,
            root.boss_hp_relief,
        );
        (re_if_win.target_mask & !re_if_win.satisfied_mask, re_if_win.distance)
    } else {
        (0u64, 0i32)
    };
    let relic_unsatisfied = unsatisfied_relic_mask != 0;
    if !pure_growth_unsatisfied && !relic_unsatisfied {
        return strat;
    }

    let has_empty_potion_slot =
        pure_growth_unsatisfied && state.player.potion_slots.iter().any(|s| s.is_none());
    let has_any_non_minion = pure_growth_unsatisfied && has_any_non_minion_living_enemy(state);

    strat.active = true;
    strat.pure_growth_unsatisfied = pure_growth_unsatisfied;
    strat.relic_unsatisfied = relic_unsatisfied;
    strat.unsatisfied_relic_mask = unsatisfied_relic_mask;
    strat.relic_distance = relic_distance;
    strat.has_empty_potion_slot = has_empty_potion_slot;
    strat.has_any_non_minion_enemy = has_any_non_minion;

    if ctx.reviving_enemies == 0 && ctx.alive_enemies > 0 {
        for p in playable {
            if blocked.contains(&p.handle) {
                continue;
            }
            let prof = state.fast_card_rollout_profile(content, p.card_id);
            if prof.card_type != CardType::Attack {
                continue;
            }
            let base_dmg = estimate_dynamic_card_base_damage(
                state,
                content,
                p.handle,
                p.card_id,
                p.upg,
                &prof,
                ctx,
            );
            if base_dmg <= 0 {
                continue;
            }
            let hits = (if p.upg { prof.upg_hits } else { prof.hits }).max(1) as i32;
            let mut per_hit = (base_dmg + ctx.player_str).max(0);
            if ctx.player_weak {
                per_hit = (per_hit * 75) / 100;
            }
            let eff_target = match p.card_id {
                CardId::Shiv => {
                    if ctx.player_has_fan_of_knives {
                        TargetType::AllEnemies
                    } else {
                        TargetType::AnyEnemy
                    }
                }
                CardId::SovereignBlade => {
                    if ctx.player_has_seeking_edge {
                        TargetType::AllEnemies
                    } else {
                        TargetType::AnyEnemy
                    }
                }
                CardId::MadScience => content
                    .card_resolved_target_type(state, p.handle)
                    .unwrap_or(prof.target_type),
                _ => prof.target_type,
            };
            let mut kills = 0i32;
            if eff_target == TargetType::AnyEnemy {
                for slot in 0..ctx.enemy_count {
                    let es = ctx.enemy_slot(slot);
                    if es.hp <= 0 {
                        continue;
                    }
                    let scaled = if es.vuln { (per_hit * 150) / 100 } else { per_hit };
                    if scaled * hits >= es.eff_hp {
                        kills = 1;
                        break;
                    }
                }
            } else if eff_target == TargetType::AllEnemies {
                for slot in 0..ctx.enemy_count {
                    let es = ctx.enemy_slot(slot);
                    if es.hp <= 0 {
                        continue;
                    }
                    let scaled = if es.vuln { (per_hit * 150) / 100 } else { per_hit };
                    if scaled * hits >= es.eff_hp {
                        kills += 1;
                    }
                }
            }
            if kills >= ctx.alive_enemies {
                strat.has_lethal_in_hand = true;
                break;
            }
        }
    }

    let mut has_unplayed_growth = false;
    let mut has_counter_or_turn_tick = (unsatisfied_relic_mask & TURN_TICK_RELIC_MASK) != 0;
    let mut growth_budget_sum = 0i32;
    let mad_science_count = state.semantic_history.growth_rewards.mad_science;
    let scan_discard = state.player.draw.len() < 5;

    let live_iter = state
        .player
        .hand
        .iter()
        .chain(state.player.draw.iter())
        .chain(if scan_discard {
            state.player.discard.iter()
        } else {
            [].iter()
        });

    for &ch in live_iter {
        let Some(c) = state.card(ch) else {
            continue;
        };
        let prof = state.fast_card_rollout_profile(content, c.id);
        if matches!(prof.card_type, CardType::Status | CardType::Curse) {
            continue;
        }
        if pure_growth_unsatisfied {
            let (b, _, _) = get_growth_card_budget(
                state,
                c,
                root,
                has_empty_potion_slot,
                has_any_non_minion,
                mad_science_count,
            );
            if b >= 0 {
                has_unplayed_growth = true;
                if growth_budget_sum < 60 {
                    growth_budget_sum += b;
                }
            }
        }
        if relic_unsatisfied && !has_counter_or_turn_tick {
            let exhausts = c.has_local_keyword(CardKeywords::EXHAUST)
                || c.temp_flags.contains(CardTempFlags::EXHAUST_ON_NEXT_PLAY);
            let is_x_s = (prof.meta_flags & mf::IS_X_STAR_COST) != 0;
            let star_cost = if is_x_s {
                state.player.stars.max(0)
            } else {
                resolve_local_star_cost_card(c, prof.canonical_star_cost as i32).max(0)
            };
            if card_advances_unsatisfied_relic(
                prof.card_type,
                exhausts,
                star_cost,
                unsatisfied_relic_mask,
            ) {
                has_counter_or_turn_tick = true;
            }
        }
    }

    let mut relic_budget_sum = 0i32;
    if relic_unsatisfied {
        for entry in &root.active_relic_targets {
            if (unsatisfied_relic_mask & (1u64 << (entry.id as u32))) != 0 && entry.hp_allowance > 0
            {
                relic_budget_sum += entry.hp_allowance;
            }
        }
    }

    let spent_so_far = (root.initial_player_hp - ctx.player_hp).max(0);
    let remaining_budget = (growth_budget_sum + relic_budget_sum - spent_so_far).max(0);
    strat.remaining_strategy_budget = remaining_budget;

    if IS_SETUP && !lethal_threat && (has_unplayed_growth || has_counter_or_turn_tick) {
        let hp_after_hit = ctx.player_hp - ctx.unblocked_incoming;
        let comfortably_safe = ctx.unblocked_incoming == 0
            || (hp_after_hit > 4.max(ctx.player_max_hp / 4)
                && ctx.unblocked_incoming <= remaining_budget);
        if comfortably_safe {
            strat.hold_last_enemy_lethal = true;
        }
    }

    strat
}

#[inline]
fn has_fatal_eligible_kill_target(
    state: &BattleState,
    ctx: &FastTurnContext,
    eff_target: TargetType,
    target_slot: Option<usize>,
    per_hit: i32,
    hits: i32,
) -> bool {
    if eff_target == TargetType::AnyEnemy {
        return target_slot.is_some_and(|slot| {
            let es = ctx.enemy_slot(slot);
            es.hp > 0 && should_enemy_death_trigger_fatal_fast(state, es.handle)
        });
    }
    if eff_target == TargetType::AllEnemies {
        for slot in 0..ctx.enemy_count {
            let es = ctx.enemy_slot(slot);
            if es.hp <= 0 {
                continue;
            }
            let scaled = if es.vuln { (per_hit * 150) / 100 } else { per_hit };
            if scaled * hits >= es.eff_hp && should_enemy_death_trigger_fatal_fast(state, es.handle)
            {
                return true;
            }
        }
        return false;
    }
    has_any_non_minion_living_enemy(state)
}

#[inline]
fn has_setup_first_relevant_cards(state: &BattleState, content: &dyn ContentDispatcher) -> bool {
    const SETUP_OPS: u8 =
        op::HAS_DRAW | op::HAS_ENGINE_OP | op::HAS_SELF_BUFF | op::HAS_ENEMY_DEBUFF;
    for &ch in state
        .player
        .hand
        .iter()
        .chain(state.player.draw.iter())
        .chain(state.player.discard.iter())
    {
        let Some(c) = state.card(ch) else {
            continue;
        };
        let prof = state.fast_card_rollout_profile(content, c.id);
        if matches!(prof.card_type, CardType::Status | CardType::Curse) {
            continue;
        }
        if prof.card_type == CardType::Power || prof.setup_weight > 0 {
            return true;
        }
        if c.has_local_keyword(CardKeywords::RETAIN)
            || c.temp_flags.contains(CardTempFlags::TEMPORARY_RETAIN_TURN)
        {
            return true;
        }
        if (prof.op_flags & SETUP_OPS) != 0 || (prof.meta_flags & mf::APPLIES_WEAK) != 0 {
            return true;
        }
    }
    false
}

#[inline(always)]
fn can_use_single_card_shortcut(
    state: &BattleState,
    root_baseline: &RootEvalBaseline,
    card_id: CardId,
    is_retained: bool,
    profile: &CardRolloutProfile,
    brightest_flame_limit: Option<i32>,
) -> bool {
    if matches!(profile.card_type, CardType::Status | CardType::Curse)
        || (profile.op_flags & op::HAS_SELF_HP_LOSS) != 0
    {
        return false;
    }
    if card_id == CardId::BrightestFlame && brightest_flame_limit.is_some() {
        return false;
    }
    if card_id == CardId::CalculatedGamble && state.player.hand.len() <= 1 {
        return false;
    }
    if matches!(
        card_id,
        CardId::Nightmare | CardId::DualWield | CardId::GrandFinale
    ) {
        return false;
    }
    if root_baseline.effective_has_growth_targets || !root_baseline.active_relic_targets.is_empty()
    {
        return false;
    }
    let has_block =
        (profile.op_flags & op::HAS_BLOCK) != 0 || (profile.meta_flags & mf::GAINS_BLOCK) != 0;
    if has_block && (is_retained || has_retain_hand_or_pyramid(state)) {
        return false;
    }
    true
}

#[allow(clippy::too_many_arguments)]
fn select_best_pilot_move(
    state: &BattleState,
    content: &dyn ContentDispatcher,
    root_baseline: &RootEvalBaseline,
    playable: &[PlayableRolloutCard],
    blocked: &[CardHandle],
    ctx: &FastTurnContext,
    pilot: PilotKind,
) -> Option<(CardHandle, Option<CreatureHandle>)> {
    match pilot {
        PilotKind::DefensiveTempo => select_best_pilot_move_impl::<false>(
            state,
            content,
            root_baseline,
            playable,
            blocked,
            ctx,
        ),
        PilotKind::SetupFirst => select_best_pilot_move_impl::<true>(
            state,
            content,
            root_baseline,
            playable,
            blocked,
            ctx,
        ),
    }
}

#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn pilot_finish_score<const IS_SETUP: bool, const IS_ANY_ENEMY: bool>(
    state: &BattleState,
    ctx: &FastTurnContext,
    pre: &MoveScorePrecomp,
    strat: &RolloutStrategyContext,
    energy_cost: i32,
    star_cost: i32,
    card_id: CardId,
    has_self_hp_loss: bool,
    common_prio: i32,
    has_enemy_debuff: bool,
    applies_weak: bool,
    eff_target: TargetType,
    retained_penalty_applies: bool,
    has_attack_op: bool,
    is_fatal_finisher: bool,
    is_playable_growth: bool,
    advances_relic: bool,
    card_growth_budget: i32,
    per_hit: i32,
    hits: i32,
    est_total_dmg: i32,
    kills_count: i32,
    killed_incoming: i32,
    target_slot: Option<usize>,
) -> i32 {
    if kills_count > 0 && kills_count >= ctx.alive_enemies && ctx.reviving_enemies == 0 {
        if !strat.active {
            return 1_000_000 + est_total_dmg * 10 - energy_cost;
        }
        if is_fatal_finisher
            && has_fatal_eligible_kill_target(state, ctx, eff_target, target_slot, per_hit, hits)
        {
            return 1_500_000 + est_total_dmg * 10 - energy_cost;
        }
        if is_playable_growth && (!strat.hold_last_enemy_lethal || !strat.relic_unsatisfied) {
            return 1_400_000 + est_total_dmg * 10 - energy_cost;
        }
        if advances_relic && strat.relic_distance <= 1 && !strat.pure_growth_unsatisfied {
            return 1_300_000 + est_total_dmg * 10 - energy_cost;
        }
        if strat.hold_last_enemy_lethal {
            return -100;
        }
        return 1_000_000 + est_total_dmg * 10 - energy_cost;
    }
    if has_self_hp_loss && pre.low_hp {
        return -500;
    }

    if strat.active && !pre.lethal_threat {
        if is_playable_growth {
            let safe_play = !pre.has_unblocked
                || (strat.has_lethal_in_hand
                    && (ctx.player_energy > energy_cost
                        || pre.unblocked <= card_growth_budget.max(0)));
            if safe_play {
                let tie_boost = if (energy_cost == 0 && star_cost == 0)
                    || card_id == CardId::ShowOff
                {
                    50
                } else {
                    0
                };
                return 1_200_000 + tie_boost - energy_cost * 100;
            }
        }
        if advances_relic {
            let safe_counter = !pre.has_unblocked
                || (strat.has_lethal_in_hand
                    && (ctx.player_energy > energy_cost
                        || pre.unblocked <= strat.remaining_strategy_budget));
            if safe_counter {
                return 1_150_000 - energy_cost * 100;
            }
        }
    }

    let mut prio = common_prio;
    if strat.active && is_fatal_finisher {
        if kills_count > 0
            && has_fatal_eligible_kill_target(state, ctx, eff_target, target_slot, per_hit, hits)
        {
            prio += 25_000 + card_growth_budget.max(0) * 200;
        } else if kills_count == 0 {
            prio -= 300;
        }
    }

    if kills_count > 0 {
        prio += kills_count * 1_600;
        if killed_incoming > 0 {
            prio += killed_incoming.min(pre.unblocked_plus_10) * pre.kill_inc_mult;
        }
    }

    if !IS_SETUP {
        if has_enemy_debuff {
            if pre.has_unblocked {
                let targets_attacker =
                    IS_ANY_ENEMY && target_slot.is_some_and(|s| ctx.enemy_incoming(s) > 0);
                prio +=
                    if targets_attacker || (!IS_ANY_ENEMY && eff_target == TargetType::AllEnemies) {
                        420
                    } else {
                        180
                    };
                if applies_weak {
                    let target_incoming = if !IS_ANY_ENEMY && eff_target == TargetType::AllEnemies {
                        pre.unblocked
                    } else if IS_ANY_ENEMY {
                        target_slot.map_or(0, |s| ctx.enemy_incoming(s))
                    } else {
                        0
                    };
                    let weak_mitigation = pre.unblocked.min((target_incoming / 4).max(0));
                    if weak_mitigation > 0 {
                        prio += weak_mitigation * pre.weak_mult;
                    }
                }
            } else {
                prio += 180;
            }
        }
        if est_total_dmg > 0 {
            prio += est_total_dmg * 45;
        }
        if IS_ANY_ENEMY {
            if let Some(slot) = target_slot {
                prio += ctx.enemy_target_bonus(slot);
            }
        }
    } else {
        if est_total_dmg > 0 {
            prio += est_total_dmg * 32;
            if retained_penalty_applies && kills_count == 0 {
                prio -= 420;
            }
        }
        if IS_ANY_ENEMY {
            if let Some(slot) = target_slot {
                let hp = ctx.enemy_hp(slot);
                if hp > 0 {
                    if has_enemy_debuff && !has_attack_op {
                        prio += hp.min(250);
                    } else {
                        prio += (200 - hp.min(200)).max(0);
                    }
                }
            }
        }
    }
    prio
}

#[derive(Clone, Copy)]
struct MoveScorePrecomp {
    unblocked: i32,
    unblocked_plus_10: i32,
    kill_inc_mult: i32,
    weak_mult: i32,
    has_unblocked: bool,
    lethal_threat: bool,
    critical_danger: bool,
    low_hp: bool,
    block_mult: i32,
    heal_bonus: i32,
    setup_retained_check: bool,
}

impl MoveScorePrecomp {
    #[inline(always)]
    fn new<const IS_SETUP: bool>(ctx: &FastTurnContext, has_non_retained: bool) -> Self {
        let unblocked = ctx.unblocked_incoming;
        let has_unblocked = unblocked > 0;
        let lethal_threat = unblocked >= ctx.player_hp;
        let post_hit_hp = ctx.player_hp - unblocked;
        let critical_danger =
            lethal_threat || (has_unblocked && post_hit_hp * 4 < ctx.player_max_hp.max(1));
        Self {
            unblocked,
            unblocked_plus_10: unblocked + 10,
            kill_inc_mult: if lethal_threat { 260 } else { 140 },
            weak_mult: if lethal_threat { 220 } else { 120 },
            has_unblocked,
            lethal_threat,
            critical_danger,
            low_hp: ctx.player_hp <= 3,
            block_mult: if lethal_threat {
                260
            } else if critical_danger {
                185
            } else {
                135
            },
            heal_bonus: if ctx.missing_hp > 0 {
                ctx.missing_hp.min(20) * 35
            } else {
                0
            },
            setup_retained_check: IS_SETUP && (has_non_retained || ctx.has_active_scaling),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BestPilotMove {
    score: i32,
    card: CardHandle,
    target: u32,
}

impl BestPilotMove {
    const EMPTY: Self = Self {
        score: 0,
        card: CardHandle(0),
        target: u32::MAX,
    };

    #[inline(always)]
    fn to_option(self) -> Option<(CardHandle, Option<CreatureHandle>)> {
        if self.score > 0 {
            Some((
                self.card,
                if self.target == u32::MAX {
                    None
                } else {
                    Some(CreatureHandle(self.target))
                },
            ))
        } else {
            None
        }
    }
}

#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn score_any_enemy_slot<const IS_SETUP: bool>(
    state: &BattleState,
    ctx: &FastTurnContext,
    pre: &MoveScorePrecomp,
    strat: &RolloutStrategyContext,
    energy_cost: i32,
    star_cost: i32,
    card_id: CardId,
    has_self_hp_loss: bool,
    common_prio: i32,
    has_enemy_debuff: bool,
    applies_weak: bool,
    eff_target: TargetType,
    retained_penalty_applies: bool,
    has_attack_op: bool,
    is_fatal_finisher: bool,
    is_playable_growth: bool,
    advances_relic: bool,
    card_growth_budget: i32,
    base_dmg: i32,
    per_hit: i32,
    hits: i32,
    single_dmg: i32,
    slot: usize,
    ch: CardHandle,
    best: &mut BestPilotMove,
) {
    let es = ctx.enemy_slot(slot);
    if es.hp <= 0 {
        return;
    }
    let (est_total_dmg, kills_count, killed_incoming) = if has_attack_op {
        if base_dmg > 0 {
            let mut kills = 0i32;
            let mut kill_inc = 0i32;
            if single_dmg >= es.eff_hp {
                kills = 1;
                kill_inc = es.incoming;
            }
            (single_dmg, kills, kill_inc)
        } else {
            (6, 0, 0)
        }
    } else {
        (0, 0, 0)
    };
    let sc = pilot_finish_score::<IS_SETUP, true>(
        state,
        ctx,
        pre,
        strat,
        energy_cost,
        star_cost,
        card_id,
        has_self_hp_loss,
        common_prio,
        has_enemy_debuff,
        applies_weak,
        eff_target,
        retained_penalty_applies,
        has_attack_op,
        is_fatal_finisher,
        is_playable_growth,
        advances_relic,
        card_growth_budget,
        per_hit,
        hits,
        est_total_dmg,
        kills_count,
        killed_incoming,
        Some(slot),
    );
    if sc > best.score {
        debug_assert_ne!(es.handle.0, u32::MAX);
        *best = BestPilotMove {
            score: sc,
            card: ch,
            target: es.handle.0,
        };
    }
}

#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn score_status_or_curse_card(
    state: &BattleState,
    ctx: &FastTurnContext,
    pre: &MoveScorePrecomp,
    ch: CardHandle,
    eff_target: TargetType,
    energy_cost: i32,
    star_cost: i32,
    exhausts: bool,
    has_self_hp_loss: bool,
    best: &mut BestPilotMove,
) {
    let status_prio = if exhausts && !has_self_hp_loss && energy_cost == 0 && star_cost == 0 {
        80
    } else if exhausts
        && !has_self_hp_loss
        && !pre.has_unblocked
        && ctx.player_energy > energy_cost
    {
        25
    } else {
        -200
    };
    if status_prio > best.score {
        match eff_target {
            TargetType::AnyEnemy => {
                if let Some(slot) = (0..ctx.enemy_count).find(|&s| ctx.enemy_hp(s) > 0) {
                    let h = ctx.enemy_handle(slot);
                    debug_assert_ne!(h.0, u32::MAX);
                    *best = BestPilotMove {
                        score: status_prio,
                        card: ch,
                        target: h.0,
                    };
                }
            }
            TargetType::AnyAlly => {
                if let Some(ah) = state.ally_handles.iter().copied().find(|&ah| {
                    ah != state.player.creature_handle
                        && state.creature(ah).is_some_and(|c| c.is_alive())
                }) {
                    debug_assert_ne!(ah.0, u32::MAX);
                    *best = BestPilotMove {
                        score: status_prio,
                        card: ch,
                        target: ah.0,
                    };
                }
            }
            TargetType::AnyPlayer => {
                if state.player_creature().is_some_and(|c| c.is_alive()) {
                    let h = state.player.creature_handle;
                    debug_assert_ne!(h.0, u32::MAX);
                    *best = BestPilotMove {
                        score: status_prio,
                        card: ch,
                        target: h.0,
                    };
                }
            }
            _ => {
                *best = BestPilotMove {
                    score: status_prio,
                    card: ch,
                    target: u32::MAX,
                };
            }
        }
    }
}

#[cold]
#[inline(never)]
fn read_genetic_algorithm_block(
    state: &BattleState,
    content: &dyn ContentDispatcher,
    ch: CardHandle,
    upg: bool,
    profile: &CardRolloutProfile,
) -> i32 {
    read_comparison_var(state, content, ch, "Block").unwrap_or_else(|| {
        if upg {
            profile.upg_block as i32
        } else {
            profile.base_block as i32
        }
    })
}

const DEFENSIVE_FLAG_BONUS: [i32; 8] = [0, 320, 340, 660, 280, 600, 620, 940];

#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn eval_and_update_best_pilot_card<const IS_SETUP: bool>(
    state: &BattleState,
    content: &dyn ContentDispatcher,
    root_baseline: &RootEvalBaseline,
    ctx: &FastTurnContext,
    pre: &MoveScorePrecomp,
    strat: &RolloutStrategyContext,
    ch: CardHandle,
    card_id: CardId,
    energy_cost: i32,
    star_cost: i32,
    upg: bool,
    exhausts: bool,
    is_retained: bool,
    profile: &CardRolloutProfile,
    best: &mut BestPilotMove,
) {
    let flags = profile.op_flags;
    let mflags = profile.meta_flags;
    let card_type = profile.card_type;
    let base_target_type = profile.target_type;
    let has_draw = (flags & op::HAS_DRAW) != 0;
    let has_energy_or_star_gain = (flags & op::HAS_ENERGY_OR_STAR_GAIN) != 0;
    let has_engine_op = (flags & op::HAS_ENGINE_OP) != 0;
    let has_self_buff = (flags & op::HAS_SELF_BUFF) != 0;
    let applies_weak = (mflags & mf::APPLIES_WEAK) != 0;
    let has_enemy_debuff = (flags & op::HAS_ENEMY_DEBUFF) != 0 || applies_weak;
    let has_heal = (mflags & mf::MAY_HEAL) != 0;
    let has_self_hp_loss = (flags & op::HAS_SELF_HP_LOSS) != 0;
    let has_block_op = (flags & op::HAS_BLOCK) != 0 || (mflags & mf::GAINS_BLOCK) != 0;
    let has_attack_op = (flags & op::HAS_ATTACK) != 0 || card_type == CardType::Attack;

    let eff_target = match card_id {
        CardId::Shiv => {
            let resolved = if ctx.player_has_fan_of_knives {
                TargetType::AllEnemies
            } else {
                TargetType::AnyEnemy
            };
            #[cfg(debug_assertions)]
            debug_assert_eq!(
                resolved,
                content
                    .card_resolved_target_type(state, ch)
                    .unwrap_or(base_target_type)
            );
            resolved
        }
        CardId::SovereignBlade => {
            let resolved = if ctx.player_has_seeking_edge {
                TargetType::AllEnemies
            } else {
                TargetType::AnyEnemy
            };
            #[cfg(debug_assertions)]
            debug_assert_eq!(
                resolved,
                content
                    .card_resolved_target_type(state, ch)
                    .unwrap_or(base_target_type)
            );
            resolved
        }
        CardId::MadScience => content
            .card_resolved_target_type(state, ch)
            .unwrap_or(base_target_type),
        _ => base_target_type,
    };

    // Avoid wasting energy on non-helpful Status/Curse cards.
    if matches!(card_type, CardType::Status | CardType::Curse) {
        score_status_or_curse_card(
            state,
            ctx,
            pre,
            ch,
            eff_target,
            energy_cost,
            star_cost,
            exhausts,
            has_self_hp_loss,
            best,
        );
        return;
    }

    // Zero-payoff & RetainHandPower hold guards (matching HotRolloutEvaluator.cs:2828-2881).
    if card_id == CardId::GrandFinale && !state.player.draw.is_empty() {
        return;
    }
    if card_id == CardId::CalculatedGamble && state.player.hand.len() <= 1 {
        return;
    }
    if matches!(card_id, CardId::Nightmare | CardId::DualWield)
        && should_hold_or_skip_copy_card(
            state,
            content,
            ch,
            card_id == CardId::DualWield,
            ctx.has_retain_hand || is_retained,
            pre.lethal_threat,
        )
    {
        return;
    }

    let mut is_fatal_finisher = false;
    let mut is_playable_growth = false;
    let mut card_growth_budget = -1i32;
    let mut advances_relic = false;
    if strat.active {
        if let Some(card_inst) = state.card(ch) {
            if strat.pure_growth_unsatisfied {
                (card_growth_budget, is_fatal_finisher, is_playable_growth) =
                    get_growth_card_budget(
                        state,
                        card_inst,
                        root_baseline,
                        strat.has_empty_potion_slot,
                        strat.has_any_non_minion_enemy,
                        state.semantic_history.growth_rewards.mad_science,
                    );
            }
            if strat.relic_unsatisfied && !has_self_hp_loss {
                advances_relic = card_advances_unsatisfied_relic(
                    card_type,
                    exhausts,
                    star_cost,
                    strat.unsatisfied_relic_mask,
                );
            }
        }
    }

    let is_pure_block_card = has_block_op
        && !has_attack_op
        && !has_draw
        && !has_energy_or_star_gain
        && !has_engine_op
        && !has_self_buff
        && !has_enemy_debuff
        && !has_heal
        && card_type != CardType::Power;
    if (ctx.has_retain_hand || is_retained)
        && !pre.has_unblocked
        && !ctx.preserves_excess_block
        && is_pure_block_card
        && !is_playable_growth
        && !advances_relic
        && state.player.hand.len() <= 7
    {
        return;
    }

    let est_block = if has_block_op {
        let raw_block = if card_id == CardId::GeneticAlgorithm {
            read_genetic_algorithm_block(state, content, ch, upg, profile)
        } else if upg {
            profile.upg_block as i32
        } else {
            profile.base_block as i32
        };
        if raw_block > 0 {
            let mut b = (raw_block + ctx.player_dex).max(0);
            if ctx.player_frail {
                let scaled = (b * 3) >> 2;
                #[cfg(debug_assertions)]
                debug_assert_eq!(scaled, (b * 75) / 100);
                b = scaled;
            }
            b
        } else {
            5
        }
    } else {
        0
    };

    let (base_dmg, hits, per_hit) = if has_attack_op {
        let base_dmg =
            estimate_dynamic_card_base_damage(state, content, ch, card_id, upg, profile, ctx);
        let hits = (if upg {
            profile.upg_hits
        } else {
            profile.hits
        })
        .max(1) as i32;
        let mut per_hit = (base_dmg + ctx.player_str).max(0);
        if base_dmg > 0 && ctx.player_weak {
            let scaled = (per_hit * 3) >> 2;
            #[cfg(debug_assertions)]
            debug_assert_eq!(scaled, (per_hit * 75) / 100);
            per_hit = scaled;
        }
        (base_dmg, hits, per_hit)
    } else {
        (0, 1, 0)
    };

    if has_self_hp_loss && pre.low_hp {
        if !(has_attack_op && base_dmg > 0 && matches!(eff_target, TargetType::AnyEnemy | TargetType::AllEnemies)) {
            return;
        }
    }

    let mut common_prio = 50i32 + if has_heal { pre.heal_bonus } else { 0 };
    if strat.active {
        if is_playable_growth && !pre.lethal_threat {
            common_prio += 1_800 + card_growth_budget.max(0) * 160;
        }
        if advances_relic && !pre.lethal_threat {
            common_prio += 650;
        }
    }
    if energy_cost == 0 && star_cost == 0 {
        if has_energy_or_star_gain {
            common_prio += 2_400;
        } else if has_draw && !has_self_hp_loss {
            common_prio += 1_800;
        } else if card_type == CardType::Power {
            common_prio += 2_200;
        } else {
            common_prio += 250;
        }
    } else if has_energy_or_star_gain && energy_cost <= 1 {
        common_prio += 1_300;
    }

    let useful_block = if pre.has_unblocked {
        est_block.min(pre.unblocked)
    } else {
        0
    };
    let retained_penalty_applies = pre.setup_retained_check && is_retained;
    if !IS_SETUP {
        if pre.has_unblocked {
            if useful_block > 0 {
                common_prio += useful_block * pre.block_mult + 450;
            } else if est_block > 0 {
                common_prio += est_block * 15;
            }
        } else if est_block > 0 {
            common_prio += est_block.min(15) * if ctx.preserves_excess_block { 28 } else { 6 };
        }
        if card_type == CardType::Power {
            let sw = profile.setup_weight as i32;
            let pw = (if sw > 0 { sw } else { 12 }).clamp(0, 50);
            common_prio += if !pre.has_unblocked {
                720 + pw * 24
            } else if !pre.critical_danger {
                360 + pw * 16
            } else if !pre.lethal_threat {
                200 + pw * 8
            } else {
                80
            };
        }
        let lut_idx = ((flags & 1) | ((flags >> 1) & 6)) as usize;
        let lut_bonus = unsafe { *DEFENSIVE_FLAG_BONUS.get_unchecked(lut_idx) };
        #[cfg(debug_assertions)]
        {
            let expected = if has_draw { 320 } else { 0 }
                + if has_engine_op { 340 } else { 0 }
                + if has_self_buff { 280 } else { 0 };
            debug_assert_eq!(lut_bonus, expected);
        }
        common_prio += lut_bonus;
        if !pre.has_unblocked && energy_cost > 0 {
            common_prio += energy_cost * 90;
        }
    } else {
        if pre.lethal_threat && useful_block > 0 {
            common_prio += useful_block * 240 + 1_200;
        }
        if card_type == CardType::Power {
            common_prio += 1_550 + (profile.setup_weight as i32).clamp(0, 50) * 25;
        }
        if has_engine_op {
            common_prio += 950;
        }
        if has_self_buff {
            common_prio += 900;
        }
        if has_draw {
            common_prio += 820;
        }
        if has_enemy_debuff {
            common_prio += 800;
        }
        if pre.has_unblocked && useful_block > 0 && !pre.lethal_threat {
            common_prio += useful_block * 70 + 220;
        }
    }
    if has_self_hp_loss && !pre.lethal_threat {
        common_prio -= 180;
    }

    match eff_target {
        TargetType::AnyEnemy => {
            if ctx.enemy_count == 1 {
                let single = if has_attack_op && base_dmg > 0 {
                    let h = if ctx.enemy_vuln(0) {
                        let vh = (per_hit * 3) >> 1;
                        #[cfg(debug_assertions)]
                        debug_assert_eq!(vh, (per_hit * 150) / 100);
                        vh
                    } else {
                        per_hit
                    };
                    h * hits
                } else {
                    0
                };
                score_any_enemy_slot::<IS_SETUP>(
                    state,
                    ctx,
                    pre,
                    strat,
                    energy_cost,
                    star_cost,
                    card_id,
                    has_self_hp_loss,
                    common_prio,
                    has_enemy_debuff,
                    applies_weak,
                    eff_target,
                    retained_penalty_applies,
                    has_attack_op,
                    is_fatal_finisher,
                    is_playable_growth,
                    advances_relic,
                    card_growth_budget,
                    base_dmg,
                    per_hit,
                    hits,
                    single,
                    0,
                    ch,
                    best,
                );
            } else if !IS_SETUP
                && !has_attack_op
                && !(pre.has_unblocked && applies_weak)
                && !strat.active
            {
                let best_slot = if pre.has_unblocked && has_enemy_debuff {
                    ctx.best_debuff_enemy_slot
                } else {
                    ctx.best_non_attack_enemy_slot
                };
                #[cfg(debug_assertions)]
                let mut dbg_best = *best;
                if let Some(slot) = best_slot {
                    score_any_enemy_slot::<IS_SETUP>(
                        state,
                        ctx,
                        pre,
                        strat,
                        energy_cost,
                        star_cost,
                        card_id,
                        has_self_hp_loss,
                        common_prio,
                        has_enemy_debuff,
                        applies_weak,
                        eff_target,
                        retained_penalty_applies,
                        false,
                        is_fatal_finisher,
                        is_playable_growth,
                        advances_relic,
                        card_growth_budget,
                        0,
                        0,
                        1,
                        0,
                        slot,
                        ch,
                        best,
                    );
                }
                #[cfg(debug_assertions)]
                {
                    for slot in 0..ctx.enemy_count {
                        score_any_enemy_slot::<IS_SETUP>(
                            state,
                            ctx,
                            pre,
                            strat,
                            energy_cost,
                            star_cost,
                            card_id,
                            has_self_hp_loss,
                            common_prio,
                            has_enemy_debuff,
                            applies_weak,
                            eff_target,
                            retained_penalty_applies,
                            false,
                            is_fatal_finisher,
                            is_playable_growth,
                            advances_relic,
                            card_growth_budget,
                            0,
                            0,
                            1,
                            0,
                            slot,
                            ch,
                            &mut dbg_best,
                        );
                    }
                    debug_assert_eq!(*best, dbg_best);
                }
            } else if !IS_SETUP && has_attack_op && base_dmg > 0 && !strat.active {
                if has_self_hp_loss && pre.low_hp {
                    return;
                }
                debug_assert!(ctx.alive_enemies >= 2);
                let dmg_normal = per_hit * hits;
                let dmg_vuln = if ctx.any_enemy_vuln {
                    let vuln_hit = (per_hit * 3) >> 1;
                    #[cfg(debug_assertions)]
                    debug_assert_eq!(vuln_hit, (per_hit * 150) / 100);
                    vuln_hit * hits
                } else {
                    dmg_normal
                };
                #[cfg(debug_assertions)]
                let mut dbg_best = *best;
                let mut local_best_sc = best.score;
                let mut local_best_handle: u32 = u32::MAX;
                for slot in 0..ctx.enemy_count {
                    let es = ctx.enemy_slot(slot);
                    if es.hp <= 0 {
                        continue;
                    }
                    let single = if es.vuln { dmg_vuln } else { dmg_normal };
                    let mut sc = common_prio + single * 45 + es.target_bonus;
                    if single >= es.eff_hp {
                        sc += 1_600;
                        if es.incoming > 0 {
                            sc += es.incoming.min(pre.unblocked_plus_10) * pre.kill_inc_mult;
                        }
                    }
                    if has_enemy_debuff {
                        if pre.has_unblocked {
                            sc += if es.incoming > 0 { 420 } else { 180 };
                            if applies_weak {
                                let weak_mit = pre.unblocked.min((es.incoming / 4).max(0));
                                if weak_mit > 0 {
                                    sc += weak_mit * pre.weak_mult;
                                }
                            }
                        } else {
                            sc += 180;
                        }
                    }
                    if sc > local_best_sc {
                        local_best_sc = sc;
                        local_best_handle = es.handle.0;
                    }
                }
                if local_best_handle != u32::MAX {
                    *best = BestPilotMove {
                        score: local_best_sc,
                        card: ch,
                        target: local_best_handle,
                    };
                }
                #[cfg(debug_assertions)]
                {
                    for slot in 0..ctx.enemy_count {
                        let single = if ctx.enemy_vuln(slot) {
                            dmg_vuln
                        } else {
                            dmg_normal
                        };
                        score_any_enemy_slot::<IS_SETUP>(
                            state,
                            ctx,
                            pre,
                            strat,
                            energy_cost,
                            star_cost,
                            card_id,
                            has_self_hp_loss,
                            common_prio,
                            has_enemy_debuff,
                            applies_weak,
                            eff_target,
                            retained_penalty_applies,
                            has_attack_op,
                            is_fatal_finisher,
                            is_playable_growth,
                            advances_relic,
                            card_growth_budget,
                            base_dmg,
                            per_hit,
                            hits,
                            single,
                            slot,
                            ch,
                            &mut dbg_best,
                        );
                    }
                    debug_assert_eq!(*best, dbg_best);
                }
            } else {
                let (dmg_normal, dmg_vuln) = if has_attack_op && base_dmg > 0 {
                    let dn = per_hit * hits;
                    let dv = if ctx.any_enemy_vuln {
                        let vuln_hit = (per_hit * 3) >> 1;
                        #[cfg(debug_assertions)]
                        debug_assert_eq!(vuln_hit, (per_hit * 150) / 100);
                        vuln_hit * hits
                    } else {
                        dn
                    };
                    (dn, dv)
                } else {
                    (0, 0)
                };
                for slot in 0..ctx.enemy_count {
                    let single = if ctx.enemy_vuln(slot) {
                        dmg_vuln
                    } else {
                        dmg_normal
                    };
                    score_any_enemy_slot::<IS_SETUP>(
                        state,
                        ctx,
                        pre,
                        strat,
                        energy_cost,
                        star_cost,
                        card_id,
                        has_self_hp_loss,
                        common_prio,
                        has_enemy_debuff,
                        applies_weak,
                        eff_target,
                        retained_penalty_applies,
                        has_attack_op,
                        is_fatal_finisher,
                        is_playable_growth,
                        advances_relic,
                        card_growth_budget,
                        base_dmg,
                        per_hit,
                        hits,
                        single,
                        slot,
                        ch,
                        best,
                    );
                }
            }
        }
        _ => {
            let target_handle = match eff_target {
                TargetType::AnyAlly => {
                    let Some(ah) = state.ally_handles.iter().copied().find(|&ah| {
                        ah != state.player.creature_handle
                            && state.creature(ah).is_some_and(|c| c.is_alive())
                    }) else {
                        return;
                    };
                    ah.0
                }
                TargetType::AnyPlayer => {
                    if !state.player_creature().is_some_and(|c| c.is_alive()) {
                        return;
                    }
                    state.player.creature_handle.0
                }
                _ => u32::MAX,
            };

            let (est_total_dmg, kills_count, killed_incoming) = if has_attack_op {
                if base_dmg > 0 {
                    if eff_target == TargetType::AllEnemies {
                        let mut total = 0i32;
                        let mut kills = 0i32;
                        let mut kill_inc = 0i32;
                        let dmg_normal = per_hit * hits;
                        let dmg_vuln = if ctx.any_enemy_vuln {
                            let vuln_hit = (per_hit * 3) >> 1;
                            #[cfg(debug_assertions)]
                            debug_assert_eq!(vuln_hit, (per_hit * 150) / 100);
                            vuln_hit * hits
                        } else {
                            dmg_normal
                        };
                        for slot in 0..ctx.enemy_count {
                            let es = ctx.enemy_slot(slot);
                            if es.hp <= 0 {
                                continue;
                            }
                            let dmg = if es.vuln { dmg_vuln } else { dmg_normal };
                            total += dmg;
                            if dmg >= es.eff_hp {
                                kills += 1;
                                kill_inc += es.incoming;
                            }
                        }
                        (total, kills, kill_inc)
                    } else {
                        (per_hit * hits, 0, 0)
                    }
                } else {
                    let total = if eff_target == TargetType::AllEnemies {
                        6 * ctx.alive_enemies.max(1)
                    } else {
                        6
                    };
                    (total, 0, 0)
                }
            } else {
                (0, 0, 0)
            };

            let sc = pilot_finish_score::<IS_SETUP, false>(
                state,
                ctx,
                pre,
                strat,
                energy_cost,
                star_cost,
                card_id,
                has_self_hp_loss,
                common_prio,
                has_enemy_debuff,
                applies_weak,
                eff_target,
                retained_penalty_applies,
                has_attack_op,
                is_fatal_finisher,
                is_playable_growth,
                advances_relic,
                card_growth_budget,
                per_hit,
                hits,
                est_total_dmg,
                kills_count,
                killed_incoming,
                None,
            );
            if sc > best.score {
                *best = BestPilotMove {
                    score: sc,
                    card: ch,
                    target: target_handle,
                };
            }
        }
    }
}

fn select_best_pilot_move_impl<const IS_SETUP: bool>(
    state: &BattleState,
    content: &dyn ContentDispatcher,
    root_baseline: &RootEvalBaseline,
    playable: &[PlayableRolloutCard],
    blocked: &[CardHandle],
    ctx: &FastTurnContext,
) -> Option<(CardHandle, Option<CreatureHandle>)> {
    let tables = state.hook_cache.cached_tables;
    let card_rollout_profiles = tables.map(|t| t.card_rollout_profiles);
    let full_tables = card_rollout_profiles.is_some_and(|rp| CardId::COUNT <= rp.len());
    let has_blocked = !blocked.is_empty();
    let use_blocked_mask = has_blocked && playable.len() <= 64;
    let blocked_mask = if use_blocked_mask {
        let mut mask = 0u64;
        for (p_idx, p) in playable.iter().enumerate() {
            if blocked.contains(&p.handle) {
                mask |= 1u64 << p_idx;
            }
        }
        mask
    } else {
        0
    };
    let mut has_non_retained = false;
    if IS_SETUP {
        for (p_idx, p) in playable.iter().enumerate() {
            if p.is_retained {
                continue;
            }
            if has_blocked {
                if use_blocked_mask {
                    if (blocked_mask & (1u64 << p_idx)) != 0 {
                        continue;
                    }
                } else if blocked.contains(&p.handle) {
                    continue;
                }
            }
            let cidx = p.card_id as usize;
            let card_type = if full_tables {
                unsafe { card_rollout_profiles.unwrap_unchecked().get_unchecked(cidx).card_type }
            } else if let Some(pr) = card_rollout_profiles.and_then(|rp| rp.get(cidx)) {
                pr.card_type
            } else if let Ok(m) = content.card_meta(p.card_id) {
                m.card_type
            } else {
                continue;
            };
            if matches!(card_type, CardType::Status | CardType::Curse) {
                continue;
            }
            has_non_retained = true;
            break;
        }
    }

    let pre = MoveScorePrecomp::new::<IS_SETUP>(ctx, has_non_retained);
    let strat = if root_baseline.effective_has_growth_targets
        || !root_baseline.active_relic_targets.is_empty()
    {
        build_rollout_strategy_context::<IS_SETUP>(
            state,
            content,
            root_baseline,
            playable,
            blocked,
            ctx,
            pre.lethal_threat,
        )
    } else {
        RolloutStrategyContext::default()
    };
    let mut best = BestPilotMove::EMPTY;

    for (p_idx, p) in playable.iter().enumerate() {
        let ch = p.handle;
        if has_blocked {
            if use_blocked_mask {
                if (blocked_mask & (1u64 << p_idx)) != 0 {
                    continue;
                }
            } else if blocked.contains(&ch) {
                continue;
            }
        }
        let card_id = p.card_id;
        let cidx = card_id as usize;
        let fallback_profile;
        let profile: &CardRolloutProfile = if full_tables {
            unsafe { card_rollout_profiles.unwrap_unchecked().get_unchecked(cidx) }
        } else if let Some(pr) = card_rollout_profiles.and_then(|rp| rp.get(cidx)) {
            pr
        } else {
            fallback_profile = content.card_rollout_profile(card_id);
            &fallback_profile
        };
        eval_and_update_best_pilot_card::<IS_SETUP>(
            state,
            content,
            root_baseline,
            ctx,
            &pre,
            &strat,
            ch,
            card_id,
            p.energy_cost.max(0),
            p.star_cost.max(0),
            p.upg,
            p.exhausts,
            p.is_retained,
            profile,
            &mut best,
        );
    }

    best.to_option()
}

#[derive(Clone, Copy)]
struct FirstDefensiveCand {
    handle: CardHandle,
    card_id: CardId,
    energy_cost: i32,
    star_cost: i32,
    upg: bool,
    exhausts: bool,
    is_retained: bool,
    prof: &'static CardRolloutProfile,
}

#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn init_defensive_ctx_and_eval_first(
    state: &BattleState,
    content: &dyn ContentDispatcher,
    root_baseline: &RootEvalBaseline,
    intent_cache: &mut IntentHitsCache,
    ctx_buf: &mut std::mem::MaybeUninit<FastTurnContext>,
    pre_buf: &mut std::mem::MaybeUninit<MoveScorePrecomp>,
    first: FirstDefensiveCand,
    best: &mut BestPilotMove,
) {
    let ctx = compute_fast_turn_context_into::<false>(
        state,
        content,
        root_baseline,
        intent_cache,
        ctx_buf,
    );
    let pre = pre_buf.write(MoveScorePrecomp::new::<false>(ctx, false));
    let strat = RolloutStrategyContext::default();
    eval_and_update_best_pilot_card::<false>(
        state,
        content,
        root_baseline,
        ctx,
        pre,
        &strat,
        first.handle,
        first.card_id,
        first.energy_cost.max(0),
        first.star_cost.max(0),
        first.upg,
        first.exhausts,
        first.is_retained,
        first.prof,
        best,
    );
}

#[inline]
fn is_brightest_flame_blocked(
    state: &BattleState,
    root_baseline: &RootEvalBaseline,
    card_id: CardId,
    brightest_flame_limit: Option<i32>,
) -> bool {
    card_id == CardId::BrightestFlame
        && brightest_flame_limit.is_some()
        && !allows_brightest_flame(
            brightest_flame_limit,
            root_baseline.initial_brightest_flame_max_hp_spent,
            root_baseline.brightest_flame_spent(state) + 2,
        )
}

#[inline]
#[allow(clippy::too_many_arguments)]
fn select_best_defensive_tempo_move_fast(
    state: &mut BattleState,
    content: &dyn ContentDispatcher,
    root_baseline: &RootEvalBaseline,
    brightest_flame_limit: Option<i32>,
    intent_cache: &mut IntentHitsCache,
    hand_buf: &mut Vec<CardHandle>,
    seen_keys: &mut Vec<RolloutDedupKey>,
) -> Option<(Option<(CardHandle, Option<CreatureHandle>)>, Option<(i32, bool)>)> {
    if state.player.hand.is_empty() || !state.is_in_progress {
        return Some((None, None));
    }
    if root_baseline.effective_has_growth_targets || !root_baseline.active_relic_targets.is_empty()
    {
        return None;
    }
    if !state.hook_cache.masks_are_valid() {
        let _ = state.ensure_hook_cache(content);
    }
    if sts2_kernel::hooks::is_over_or_ending_checked(state, content).unwrap_or(true) {
        return Some((None, None));
    }
    let (fast_query_safe, comb, fast_energy_hook, fast_should_play) =
        resolve_fast_query_and_hooks(state, content);
    if !fast_query_safe || fast_energy_hook == FastEnergyHook::Fallback {
        return None;
    }

    let (tables, full_profiles, cards_contiguous, has_hex_power, bound_blocked) =
        resolve_fast_scan_meta(state, comb, fast_energy_hook, fast_should_play);
    let player_energy = state.player.energy;
    let player_stars = state.player.stars;
    let mut seen_fast = [0u64; 16];
    let mut seen_fast_len = 0usize;
    let mut use_seen_keys = false;

    hand_buf.clear();
    hand_buf.extend_from_slice(&state.player.hand);

    let mut first_cand: Option<FirstDefensiveCand> = None;
    let mut has_ctx = false;
    let mut ctx_buf = std::mem::MaybeUninit::<FastTurnContext>::uninit();
    let mut pre_buf = std::mem::MaybeUninit::<MoveScorePrecomp>::uninit();
    let mut best = BestPilotMove::EMPTY;
    let strat = RolloutStrategyContext::default();

    let hand_len = hand_buf.len();
    #[cfg(target_arch = "x86_64")]
    if hand_len >= 4 {
        for &ahead_ch in &hand_buf[..3] {
            let ahead = ahead_ch.0.wrapping_sub(1) as usize;
            if ahead < state.cards.len() {
                unsafe {
                    core::arch::x86_64::_mm_prefetch::<{ core::arch::x86_64::_MM_HINT_T0 }>(
                        state.cards.as_ptr().add(ahead).cast::<i8>(),
                    );
                }
            }
        }
    }

    for hi in 0..hand_len {
        let ch = hand_buf[hi];
        #[cfg(target_arch = "x86_64")]
        if hand_len >= 4 && hi + 3 < hand_len {
            let ahead = unsafe { *hand_buf.get_unchecked(hi + 3) }
                .0
                .wrapping_sub(1) as usize;
            if ahead < state.cards.len() {
                unsafe {
                    core::arch::x86_64::_mm_prefetch::<{ core::arch::x86_64::_MM_HINT_T0 }>(
                        state.cards.as_ptr().add(ahead).cast::<i8>(),
                    );
                }
            }
        }
        let Some(c) = try_extract_fast_playable_hand_card(
            state,
            content,
            comb,
            fast_energy_hook,
            tables,
            full_profiles,
            cards_contiguous,
            player_energy,
            player_stars,
            has_hex_power,
            bound_blocked,
            ch,
            &mut seen_fast,
            &mut seen_fast_len,
            &mut use_seen_keys,
            seen_keys,
        ) else {
            continue;
        };
        if is_brightest_flame_blocked(state, root_baseline, c.card_id, brightest_flame_limit) {
            continue;
        }

        if !has_ctx {
            if let Some(first) = first_cand.take() {
                init_defensive_ctx_and_eval_first(
                    state,
                    content,
                    root_baseline,
                    intent_cache,
                    &mut ctx_buf,
                    &mut pre_buf,
                    first,
                    &mut best,
                );
                has_ctx = true;
            } else {
                first_cand = Some(FirstDefensiveCand {
                    handle: c.handle,
                    card_id: c.card_id,
                    energy_cost: c.energy_cost,
                    star_cost: c.star_cost,
                    upg: c.upg,
                    exhausts: c.exhausts,
                    is_retained: c.is_retained,
                    prof: c.prof,
                });
                continue;
            }
        }
        let ctx = unsafe { ctx_buf.assume_init_ref() };
        let pre = unsafe { pre_buf.assume_init_ref() };
        eval_and_update_best_pilot_card::<false>(
            state,
            content,
            root_baseline,
            ctx,
            pre,
            &strat,
            c.handle,
            c.card_id,
            c.energy_cost.max(0),
            c.star_cost.max(0),
            c.upg,
            c.exhausts,
            c.is_retained,
            c.prof,
            &mut best,
        );
    }

    if has_ctx {
        let ctx = unsafe { ctx_buf.assume_init_ref() };
        return Some((best.to_option(), Some((ctx.unblocked_incoming, ctx.has_retain_hand))));
    }
    let Some(p) = first_cand else {
        return Some((None, None));
    };
    if p.energy_cost < 1_000_000 {
        let profile = p.prof;
        if can_use_single_card_shortcut(
            state,
            root_baseline,
            p.card_id,
            p.is_retained,
            profile,
            brightest_flame_limit,
        ) {
            let eff_target = match p.card_id {
                CardId::Shiv | CardId::SovereignBlade | CardId::MadScience => content
                    .card_resolved_target_type(state, p.handle)
                    .unwrap_or(profile.target_type),
                _ => profile.target_type,
            };
            let fast_pick = match eff_target {
                TargetType::AnyEnemy => {
                    let mut sole_enemy: Option<CreatureHandle> = None;
                    let mut living = 0usize;
                    for &eh in &state.enemy_handles {
                        if state.creature(eh).is_some_and(|c| c.is_alive()) {
                            sole_enemy = Some(eh);
                            living += 1;
                            if living > 1 {
                                break;
                            }
                        }
                    }
                    if living == 1 {
                        Some((p.handle, sole_enemy))
                    } else {
                        None
                    }
                }
                TargetType::AnyAlly => None,
                TargetType::AnyPlayer => {
                    if state.player_creature().is_some_and(|c| c.is_alive()) {
                        Some((p.handle, Some(state.player.creature_handle)))
                    } else {
                        None
                    }
                }
                _ => Some((p.handle, None)),
            };
            if let Some(pick) = fast_pick {
                return Some((Some(pick), None));
            }
        }
    }
    init_defensive_ctx_and_eval_first(
        state,
        content,
        root_baseline,
        intent_cache,
        &mut ctx_buf,
        &mut pre_buf,
        p,
        &mut best,
    );
    let ctx = unsafe { ctx_buf.assume_init_ref() };
    Some((best.to_option(), Some((ctx.unblocked_incoming, ctx.has_retain_hand))))
}

#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn collect_and_select_pilot_move_slow(
    state: &mut BattleState,
    content: &dyn ContentDispatcher,
    root_baseline: &RootEvalBaseline,
    brightest_flame_limit: Option<i32>,
    intent_cache: &mut IntentHitsCache,
    pilot: PilotKind,
    blocked_buf: &[CardHandle],
    playable_buf: &mut Vec<PlayableRolloutCard>,
    hand_buf: &mut Vec<CardHandle>,
    seen_keys: &mut Vec<RolloutDedupKey>,
) -> (Option<(CardHandle, Option<CreatureHandle>)>, Option<(i32, bool)>) {
    collect_rollout_playable_cards(state, content, hand_buf, seen_keys, playable_buf);
    if brightest_flame_limit.is_some() {
        playable_buf.retain(|p| {
            !is_brightest_flame_blocked(state, root_baseline, p.card_id, brightest_flame_limit)
        });
    }
    if playable_buf.is_empty() {
        return (None, None);
    }
    let fast_pick = if pilot == PilotKind::DefensiveTempo
        && playable_buf.len() == 1
        && blocked_buf.is_empty()
    {
        let p = playable_buf[0];
        if p.energy_cost < 1_000_000 {
            let profile = state.fast_card_rollout_profile(content, p.card_id);
            if can_use_single_card_shortcut(
                state,
                root_baseline,
                p.card_id,
                p.is_retained,
                &profile,
                brightest_flame_limit,
            ) {
                let eff_target = match p.card_id {
                    CardId::Shiv | CardId::SovereignBlade | CardId::MadScience => content
                        .card_resolved_target_type(state, p.handle)
                        .unwrap_or(profile.target_type),
                    _ => profile.target_type,
                };
                match eff_target {
                    TargetType::AnyEnemy => {
                        let mut sole_enemy: Option<CreatureHandle> = None;
                        let mut living = 0usize;
                        for &eh in &state.enemy_handles {
                            if state.creature(eh).is_some_and(|c| c.is_alive()) {
                                sole_enemy = Some(eh);
                                living += 1;
                                if living > 1 {
                                    break;
                                }
                            }
                        }
                        if living == 1 {
                            Some((p.handle, sole_enemy))
                        } else {
                            None
                        }
                    }
                    TargetType::AnyAlly => None,
                    TargetType::AnyPlayer => {
                        if state.player_creature().is_some_and(|c| c.is_alive()) {
                            Some((p.handle, Some(state.player.creature_handle)))
                        } else {
                            None
                        }
                    }
                    _ => Some((p.handle, None)),
                }
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };
    if let Some(pick) = fast_pick {
        #[cfg(debug_assertions)]
        {
            let dbg_ctx =
                compute_fast_turn_context::<false>(state, content, root_baseline, intent_cache);
            debug_assert_eq!(
                Some(pick),
                select_best_pilot_move_impl::<false>(
                    state,
                    content,
                    root_baseline,
                    playable_buf,
                    blocked_buf,
                    &dbg_ctx,
                )
            );
        }
        return (Some(pick), None);
    }
    let ctx = if pilot == PilotKind::DefensiveTempo {
        compute_fast_turn_context::<false>(state, content, root_baseline, intent_cache)
    } else {
        compute_fast_turn_context::<true>(state, content, root_baseline, intent_cache)
    };
    let mv = select_best_pilot_move(
        state,
        content,
        root_baseline,
        playable_buf,
        blocked_buf,
        &ctx,
        pilot,
    );
    (mv, Some((ctx.unblocked_incoming, ctx.has_retain_hand)))
}

struct Turn1ProbeCacheHook<'a, 'b> {
    probe_cache: &'a mut ProbeCacheAccess<'b>,
    l1: &'a mut ChoiceProbeL1,
    cfg_hash_1t: u64,
    hit: Option<(usize, CachedProbeEntry)>,
    miss_key: Option<(StateHash, usize)>,
}

#[allow(clippy::too_many_arguments)]
fn run_pilot_turns<const TRACK_AUTO_POTIONS: bool, R>(
    state: &mut BattleState,
    compact_steps: &mut Vec<CompactRolloutStep>,
    choice_slab: &mut RolloutChoiceSlab,
    choice_scored_buf: &mut Vec<(usize, i32, u32)>,
    sel_buf: &mut SmallVec<[usize; 16]>,
    intent_cache: &mut IntentHitsCache,
    mut restore_base: R,
    content: &dyn ContentDispatcher,
    root_baseline: &RootEvalBaseline,
    brightest_flame_limit: Option<i32>,
    pilot: PilotKind,
    horizon_turns: usize,
    blocked_buf: &mut SmallVec<[CardHandle; 8]>,
    empty_q: &mut SmallVec<[Vec<usize>; 2]>,
    playable_buf: &mut Vec<PlayableRolloutCard>,
    hand_buf: &mut Vec<CardHandle>,
    seen_keys: &mut Vec<RolloutDedupKey>,
    mut turn1_hook: Option<&mut Turn1ProbeCacheHook<'_, '_>>,
) -> Option<(usize, u16)>
where
    R: FnMut(&mut BattleState) -> bool,
{
    compact_steps.clear();
    choice_slab.clear();
    let mut total_steps = 0usize;
    let mut auto_potions_used = 0u16;
    let mut cur_potions = if TRACK_AUTO_POTIONS {
        count_non_empty_potion_slots(state)
    } else {
        0
    };

    for turn in 0..horizon_turns {
        if !state.is_in_progress
            || state.won
            || state.battleworn_dummy_timed_out
            || !state.has_living_player_creature()
            || total_steps >= MAX_TOTAL_ROLLOUT_STEPS
        {
            break;
        }
        if turn == 1 {
            if let Some(hook) = turn1_hook.as_deref_mut() {
                let h1 = hash_canonical_key(state);
                if let Some(Some(entry1)) =
                    hook.probe_cache.get_with_l1(hook.l1, h1, hook.cfg_hash_1t)
                {
                    hook.hit = Some((total_steps, entry1));
                    return Some((total_steps, auto_potions_used));
                }
                hook.miss_key = Some((h1, total_steps));
            }
        }

        blocked_buf.clear();
        for _step in 0..MAX_ROLLOUT_ACTIONS_PER_TURN {
            if !state.is_in_progress
                || state.won
                || state.player.phase != PlayerTurnPhase::Play
                || total_steps >= MAX_TOTAL_ROLLOUT_STEPS
            {
                break;
            }
            let fast_mv_opt = if pilot == PilotKind::DefensiveTempo && blocked_buf.is_empty() {
                select_best_defensive_tempo_move_fast(
                    state,
                    content,
                    root_baseline,
                    brightest_flame_limit,
                    intent_cache,
                    hand_buf,
                    seen_keys,
                )
            } else {
                None
            };
            let (ch, tgt, ctx_choice_info) = match fast_mv_opt {
                Some((fast_mv, c_info)) => {
                    #[cfg(debug_assertions)]
                    {
                        collect_rollout_playable_cards(
                            state,
                            content,
                            hand_buf,
                            seen_keys,
                            playable_buf,
                        );
                        if brightest_flame_limit.is_some() {
                            playable_buf.retain(|p| {
                                !is_brightest_flame_blocked(
                                    state,
                                    root_baseline,
                                    p.card_id,
                                    brightest_flame_limit,
                                )
                            });
                        }
                        let dbg_ctx = compute_fast_turn_context::<false>(
                            state,
                            content,
                            root_baseline,
                            intent_cache,
                        );
                        let dbg_mv = select_best_pilot_move_impl::<false>(
                            state,
                            content,
                            root_baseline,
                            playable_buf,
                            blocked_buf,
                            &dbg_ctx,
                        );
                        debug_assert_eq!(fast_mv, dbg_mv);
                    }
                    let Some(pick) = fast_mv else {
                        break;
                    };
                    (pick.0, pick.1, c_info)
                }
                None => {
                    let (mv_opt, c_info) = collect_and_select_pilot_move_slow(
                        state,
                        content,
                        root_baseline,
                        brightest_flame_limit,
                        intent_cache,
                        pilot,
                        blocked_buf,
                        playable_buf,
                        hand_buf,
                        seen_keys,
                    );
                    let Some(pick) = mv_opt else {
                        break;
                    };
                    (pick.0, pick.1, c_info)
                }
            };

            let step_start_turn = state.player.turn_number;
            empty_q.clear();
            let mut resolved =
                match CombatEngine::play_card_prechecked(state, content, ch, tgt, empty_q) {
                    Ok(outcome) => resolve_step_choices(
                        state,
                        content,
                        root_baseline,
                        step_start_turn,
                        ctx_choice_info,
                        outcome,
                        choice_slab,
                        choice_scored_buf,
                        sel_buf,
                    ),
                    Err(_) => None,
                };
            if resolved.is_some()
                && brightest_flame_limit.is_some()
                && !allows_brightest_flame(
                    brightest_flame_limit,
                    root_baseline.initial_brightest_flame_max_hp_spent,
                    root_baseline.brightest_flame_spent(state),
                )
            {
                resolved = None;
            }
            let Some((choices_offset, choices_len)) = resolved else {
                if !restore_base(state) {
                    return None;
                }
                let replayed_pots = replay_compact_steps(
                    state,
                    content,
                    compact_steps,
                    choice_slab,
                    empty_q,
                    sel_buf,
                )
                .ok()?;
                if TRACK_AUTO_POTIONS {
                    auto_potions_used = replayed_pots;
                    cur_potions = count_non_empty_potion_slots(state);
                }
                blocked_buf.push(ch);
                continue;
            };
            if TRACK_AUTO_POTIONS {
                let potions_after = count_non_empty_potion_slots(state);
                auto_potions_used += cur_potions.saturating_sub(potions_after) as u16;
                cur_potions = potions_after;
            }

            total_steps += 1;
            compact_steps.push(CompactRolloutStep {
                card_handle: ch.0,
                target_handle: tgt.map_or(u32::MAX, |h| h.0),
                choices_offset,
                choices_len,
                is_end_turn: 0,
                _pad: 0,
            });
        }

        if !state.is_in_progress
            || state.won
            || !state.has_living_player_creature()
            || state.player.phase != PlayerTurnPhase::Play
            || total_steps >= MAX_TOTAL_ROLLOUT_STEPS
        {
            break;
        }

        let end_start_turn = state.player.turn_number;
        let Ok(end_out) = CombatEngine::end_player_turn(state, content) else {
            return None;
        };
        let Some((choices_offset, choices_len)) = resolve_step_choices(
            state,
            content,
            root_baseline,
            end_start_turn,
            None,
            end_out,
            choice_slab,
            choice_scored_buf,
            sel_buf,
        ) else {
            return None;
        };
        if brightest_flame_limit.is_some()
            && !allows_brightest_flame(
                brightest_flame_limit,
                root_baseline.initial_brightest_flame_max_hp_spent,
                root_baseline.brightest_flame_spent(state),
            )
        {
            return None;
        }
        if TRACK_AUTO_POTIONS {
            let potions_after = count_non_empty_potion_slots(state);
            auto_potions_used += cur_potions.saturating_sub(potions_after) as u16;
            cur_potions = potions_after;
        }
        total_steps += 1;
        compact_steps.push(CompactRolloutStep {
            card_handle: 0,
            target_handle: u32::MAX,
            choices_offset,
            choices_len,
            is_end_turn: 1,
            _pad: 0,
        });
    }

    Some((total_steps, auto_potions_used))
}

#[allow(clippy::too_many_arguments)]
fn finish_choice_probe_eval(
    eval_state: &BattleState,
    intent_cache: &mut IntentHitsCache,
    l1: &mut ChoiceProbeL1,
    exact_state_hash: StateHash,
    cfg_hash: u64,
    cfg_hash_1t: u64,
    probe_cache: &mut ProbeCacheAccess<'_>,
    content: &dyn ContentDispatcher,
    root_baseline: &RootEvalBaseline,
    profile: &SolverSearchProfile,
    eval_mode: EvalMode,
    run_opt: Option<(usize, u16)>,
    turn1_hit: Option<(usize, CachedProbeEntry)>,
    turn1_miss_key: Option<(StateHash, usize)>,
    optional_paid_potion_cost: i32,
    optional_paid_potion_count: usize,
    parent_action_count: u16,
) -> Option<ChoiceProbeResult> {
    let has_any_strategy_targets =
        root_baseline.effective_has_growth_targets || !root_baseline.active_relic_targets.is_empty();
    if run_opt.is_some() {
        if let Some((turn0_steps, raw_entry1)) = turn1_hit {
            #[cfg(debug_assertions)]
            {
                let dbg_s1 = eval_state.clone();
                let mut dbg_state = dbg_s1.clone();
                let mut dbg_steps = Vec::new();
                let mut dbg_slab = RolloutChoiceSlab::default();
                let mut dbg_scored = Vec::new();
                let mut dbg_sel = SmallVec::new();
                let mut dbg_intent = IntentHitsCache::new();
                let mut dbg_blocked = SmallVec::new();
                let mut dbg_eq = SmallVec::new();
                let mut dbg_play = Vec::new();
                let mut dbg_hand = Vec::new();
                let mut dbg_keys = Vec::new();
                let dbg_turn1 = run_pilot_turns::<false, _>(
                    &mut dbg_state,
                    &mut dbg_steps,
                    &mut dbg_slab,
                    &mut dbg_scored,
                    &mut dbg_sel,
                    &mut dbg_intent,
                    |s| {
                        clone_battle_state_into(&dbg_s1, s);
                        true
                    },
                    content,
                    root_baseline,
                    None,
                    PilotKind::DefensiveTempo,
                    1,
                    &mut dbg_blocked,
                    &mut dbg_eq,
                    &mut dbg_play,
                    &mut dbg_hand,
                    &mut dbg_keys,
                    None,
                );
                let dbg_steps1 = dbg_turn1.map_or(0, |(st, _)| st);
                let dbg_fast = evaluate_fast(&dbg_state, content, root_baseline, None);
                let dbg_event_defeat = dbg_fast.is_event_defeat;
                let dbg_win =
                    dbg_fast.won && !dbg_state.battleworn_dummy_timed_out && !dbg_event_defeat;
                let dbg_loss =
                    !dbg_win && (dbg_fast.player_dead || dbg_fast.projected_player_hp <= 0);
                let dbg_event_defeat_only = !dbg_win && !dbg_loss && dbg_event_defeat;
                let dbg_full = if dbg_win || dbg_loss || dbg_event_defeat_only {
                    FullEvalDetails::default()
                } else {
                    crate::eval::evaluate_full_details_cached(
                        &dbg_state,
                        content,
                        &dbg_fast,
                        root_baseline,
                        Some(&mut dbg_intent),
                    )
                };
                let dbg_lin = !(profile.base_score_only || dbg_loss || dbg_event_defeat_only);
                let dbg_scores1 = compute_scores_at_zero(
                    &dbg_fast,
                    &dbg_full,
                    root_baseline,
                    profile,
                    dbg_lin,
                    dbg_steps1.min(u16::MAX as usize) as u16,
                );
                debug_assert_eq!(raw_entry1.hp_lost, dbg_fast.cumulative_hp_lost);
                debug_assert_eq!(raw_entry1.setup_score, dbg_full.setup_score);
                debug_assert_eq!(raw_entry1.board_investment, dbg_full.board_investment);
                debug_assert_eq!(raw_entry1.alchemize_count, dbg_fast.alchemize_count);
                debug_assert_eq!(raw_entry1.empty_potion_slots, dbg_fast.empty_potion_slots);
                debug_assert_eq!(raw_entry1.is_victory, dbg_win);
                debug_assert_eq!(raw_entry1.is_defeat, dbg_loss);
                debug_assert_eq!(raw_entry1.is_event_defeat, dbg_event_defeat_only);
                debug_assert_eq!(raw_entry1.linear_adj, dbg_lin);
                for m in 0..4 {
                    debug_assert!((dbg_scores1[m] - raw_entry1.scores_at_zero[m]).abs() < 1e-6);
                }
            }
            let mut entry0 = raw_entry1;
            if entry0.linear_adj && turn0_steps > 0 {
                let act_coeff = if entry0.is_victory { 10.0 } else { 5.0 };
                let adj = (turn0_steps as f64) * act_coeff;
                for s in &mut entry0.scores_at_zero {
                    *s -= adj;
                }
            }
            probe_cache.insert_with_l1(l1, exact_state_hash, cfg_hash, Some(entry0));
            return Some(entry0.materialize_result(
                root_baseline,
                eval_mode,
                optional_paid_potion_cost,
                optional_paid_potion_count,
                parent_action_count,
            ));
        }
    }

    let total_steps = run_opt.map_or(0, |(steps, _)| steps);
    let (fast, full) = if !eval_state.is_in_progress
        || eval_state.won
        || eval_state.battleworn_dummy_timed_out
        || !eval_state.has_living_player_creature()
    {
        (
            evaluate_fast(eval_state, content, root_baseline, None),
            FullEvalDetails::default(),
        )
    } else {
        let (f, full_det, _) = evaluate_fast_and_full_fused(
            eval_state,
            content,
            root_baseline,
            0,
            0,
            None,
            Some(intent_cache),
        );
        if f.won || f.player_dead || f.projected_player_hp <= 0 || f.is_event_defeat {
            (f, FullEvalDetails::default())
        } else {
            (f, full_det)
        }
    };
    let is_victory = fast.won && !eval_state.battleworn_dummy_timed_out && !fast.is_event_defeat;
    let is_defeat = !is_victory && (fast.player_dead || fast.projected_player_hp <= 0);
    let is_event_defeat = !is_victory && !is_defeat && fast.is_event_defeat;
    let linear_adj = !(profile.base_score_only || is_defeat || is_event_defeat);
    let probe_steps = total_steps.min(u16::MAX as usize) as u16;
    let scores_at_zero =
        compute_scores_at_zero(&fast, &full, root_baseline, profile, linear_adj, probe_steps);
    let (strategy_goal_hp_credit, strategic_hp_credit) = if has_any_strategy_targets {
        (fast.strategy_goal_hp_credit, fast.strategic_hp_credit)
    } else {
        (0, 0)
    };
    let entry = CachedProbeEntry {
        scores_at_zero,
        hp_lost: fast.cumulative_hp_lost,
        setup_score: full.setup_score,
        board_investment: full.board_investment,
        strategy_goal_hp_credit,
        strategic_hp_credit,
        alchemize_count: fast.alchemize_count,
        empty_potion_slots: fast.empty_potion_slots,
        is_victory,
        is_defeat,
        is_event_defeat,
        linear_adj,
    };
    probe_cache.insert_with_l1(l1, exact_state_hash, cfg_hash, Some(entry));
    if run_opt.is_some() {
        if let Some((h1, turn0_steps)) = turn1_miss_key {
            let turn1_steps = total_steps.saturating_sub(turn0_steps).min(u16::MAX as usize) as u16;
            let scores1 = compute_scores_at_zero(
                &fast,
                &full,
                root_baseline,
                profile,
                linear_adj,
                turn1_steps,
            );
            let entry1 = CachedProbeEntry {
                scores_at_zero: scores1,
                ..entry
            };
            probe_cache.insert_with_l1(l1, h1, cfg_hash_1t, Some(entry1));
        }
    }
    Some(entry.materialize_result(
        root_baseline,
        eval_mode,
        optional_paid_potion_cost,
        optional_paid_potion_count,
        parent_action_count,
    ))
}

#[allow(clippy::too_many_arguments)]
fn evaluate_choice_probe_in_place_core<R>(
    ws: &mut ChoiceProbeWorkspace,
    state: &mut BattleState,
    restore_base: R,
    exact_state_hash: StateHash,
    probe_cache: &mut ProbeCacheAccess<'_>,
    content: &dyn ContentDispatcher,
    root_baseline: &RootEvalBaseline,
    profile: &SolverSearchProfile,
    eval_mode: EvalMode,
    max_turns_ahead: usize,
    optional_paid_potion_cost: i32,
    optional_paid_potion_count: usize,
    parent_action_count: u16,
) -> Option<ChoiceProbeResult>
where
    R: FnMut(&mut BattleState) -> bool,
{
    let cache_turns = max_turns_ahead.min(4) as u8;
    let cfg_hash = choice_probe_cfg_hash(profile, cache_turns);
    if let Some(cached) = probe_cache.get_with_l1(&mut ws.l1, exact_state_hash, cfg_hash) {
        return cached.map(|e| {
            e.materialize_result(
                root_baseline,
                eval_mode,
                optional_paid_potion_cost,
                optional_paid_potion_count,
                parent_action_count,
            )
        });
    }
    let cfg_hash_1t = choice_probe_cfg_hash(profile, 0);
    let turn_slots = (cache_turns as usize) + 1;
    let mut turn1_hook = (cache_turns == 1).then(|| Turn1ProbeCacheHook {
        probe_cache,
        l1: &mut ws.l1,
        cfg_hash_1t,
        hit: None,
        miss_key: None,
    });
    let run_opt = run_pilot_turns::<false, _>(
        state,
        &mut ws.compact_steps,
        &mut ws.choice_slab,
        &mut ws.choice_scored_buf,
        &mut ws.sel_buf,
        &mut ws.intent_cache,
        restore_base,
        content,
        root_baseline,
        None,
        PilotKind::DefensiveTempo,
        turn_slots,
        &mut ws.blocked_buf,
        &mut ws.empty_q,
        &mut ws.playable_buf,
        &mut ws.hand_buf,
        &mut ws.seen_keys,
        turn1_hook.as_mut(),
    );
    let (turn1_hit, turn1_miss_key) = match turn1_hook {
        Some(h) => (h.hit, h.miss_key),
        None => (None, None),
    };
    finish_choice_probe_eval(
        state,
        &mut ws.intent_cache,
        &mut ws.l1,
        exact_state_hash,
        cfg_hash,
        cfg_hash_1t,
        probe_cache,
        content,
        root_baseline,
        profile,
        eval_mode,
        run_opt,
        turn1_hit,
        turn1_miss_key,
        optional_paid_potion_cost,
        optional_paid_potion_count,
        parent_action_count,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn evaluate_choice_probe_in_place(
    ws: &mut ChoiceProbeWorkspace,
    state: &mut BattleState,
    packed_fallback: &[u8],
    exact_state_hash: StateHash,
    probe_cache: &mut ProbeCacheAccess<'_>,
    static_ctx: &Arc<RootStaticContext>,
    content: &dyn ContentDispatcher,
    root_baseline: &RootEvalBaseline,
    profile: &SolverSearchProfile,
    eval_mode: EvalMode,
    max_turns_ahead: usize,
    optional_paid_potion_cost: i32,
    optional_paid_potion_count: usize,
    parent_action_count: u16,
) -> Option<ChoiceProbeResult> {
    evaluate_choice_probe_in_place_core(
        ws,
        state,
        // SAFETY: `packed_fallback` was produced by `pack_state` in this process.
        |s| unsafe { unpack_state(packed_fallback, static_ctx, s) }.is_ok(),
        exact_state_hash,
        probe_cache,
        content,
        root_baseline,
        profile,
        eval_mode,
        max_turns_ahead,
        optional_paid_potion_cost,
        optional_paid_potion_count,
        parent_action_count,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn evaluate_choice_probe_in_place_with_restore<R>(
    ws: &mut ChoiceProbeWorkspace,
    state: &mut BattleState,
    restore_base: R,
    exact_state_hash: StateHash,
    probe_cache: &mut ProbeCacheAccess<'_>,
    content: &dyn ContentDispatcher,
    root_baseline: &RootEvalBaseline,
    profile: &SolverSearchProfile,
    eval_mode: EvalMode,
    max_turns_ahead: usize,
    optional_paid_potion_cost: i32,
    optional_paid_potion_count: usize,
    parent_action_count: u16,
) -> Option<ChoiceProbeResult>
where
    R: FnMut(&mut BattleState) -> bool,
{
    evaluate_choice_probe_in_place_core(
        ws,
        state,
        restore_base,
        exact_state_hash,
        probe_cache,
        content,
        root_baseline,
        profile,
        eval_mode,
        max_turns_ahead,
        optional_paid_potion_cost,
        optional_paid_potion_count,
        parent_action_count,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn evaluate_choice_probe_packed(
    ws: &mut ChoiceProbeWorkspace,
    packed_state: &[u8],
    exact_state_hash: StateHash,
    probe_cache: &mut ProbeCacheAccess<'_>,
    static_ctx: &Arc<RootStaticContext>,
    content: &dyn ContentDispatcher,
    root_baseline: &RootEvalBaseline,
    profile: &SolverSearchProfile,
    eval_mode: EvalMode,
    max_turns_ahead: usize,
    optional_paid_potion_cost: i32,
    optional_paid_potion_count: usize,
    parent_action_count: u16,
) -> Option<ChoiceProbeResult> {
    let cache_turns = max_turns_ahead.min(4) as u8;
    let cfg_hash = choice_probe_cfg_hash(profile, cache_turns);
    if let Some(cached) = probe_cache.get_with_l1(&mut ws.l1, exact_state_hash, cfg_hash) {
        return cached.map(|e| {
            e.materialize_result(
                root_baseline,
                eval_mode,
                optional_paid_potion_cost,
                optional_paid_potion_count,
                parent_action_count,
            )
        });
    }
    // SAFETY: `packed_state` was produced by `pack_state` in this process.
    if unsafe { unpack_state(packed_state, static_ctx, &mut ws.probe_state) }.is_err() {
        probe_cache.insert_with_l1(&mut ws.l1, exact_state_hash, cfg_hash, None);
        return None;
    }
    let cfg_hash_1t = choice_probe_cfg_hash(profile, 0);
    let turn_slots = (cache_turns as usize) + 1;
    let mut turn1_hook = (cache_turns == 1).then(|| Turn1ProbeCacheHook {
        probe_cache,
        l1: &mut ws.l1,
        cfg_hash_1t,
        hit: None,
        miss_key: None,
    });
    let run_opt = run_pilot_turns::<false, _>(
        &mut ws.probe_state,
        &mut ws.compact_steps,
        &mut ws.choice_slab,
        &mut ws.choice_scored_buf,
        &mut ws.sel_buf,
        &mut ws.intent_cache,
        // SAFETY: `packed_state` was produced by `pack_state` in this process.
        |s| unsafe { unpack_state(packed_state, static_ctx, s) }.is_ok(),
        content,
        root_baseline,
        None,
        PilotKind::DefensiveTempo,
        turn_slots,
        &mut ws.blocked_buf,
        &mut ws.empty_q,
        &mut ws.playable_buf,
        &mut ws.hand_buf,
        &mut ws.seen_keys,
        turn1_hook.as_mut(),
    );
    let (turn1_hit, turn1_miss_key) = match turn1_hook {
        Some(h) => (h.hit, h.miss_key),
        None => (None, None),
    };
    finish_choice_probe_eval(
        &ws.probe_state,
        &mut ws.intent_cache,
        &mut ws.l1,
        exact_state_hash,
        cfg_hash,
        cfg_hash_1t,
        probe_cache,
        content,
        root_baseline,
        profile,
        eval_mode,
        run_opt,
        turn1_hit,
        turn1_miss_key,
        optional_paid_potion_cost,
        optional_paid_potion_count,
        parent_action_count,
    )
}

#[inline]
pub(crate) fn encode_choice_probe_rank_score(probe: &ChoiceProbeResult) -> f64 {
    let s = probe
        .score
        .clamp(-1_000_000_000_000_000.0, 1_000_000_000_000_000.0);
    if probe.is_victory {
        2_000_000_000_000_000.0 + s - (probe.hp_lost as f64) * 100_000.0
            + (probe.strategy_goal_hp_credit as f64) * 100_000.0
    } else if probe.is_defeat {
        -2_000_000_000_000_000.0 + s
    } else if probe.is_event_defeat {
        -1_500_000_000_000_000.0 + s
    } else {
        s
    }
}

#[inline(always)]
fn is_better_victory(
    b_entry: &TurnRolloutPilotEntry,
    b_v_centi: i64,
    b_score: f64,
    a_entry: &TurnRolloutPilotEntry,
    a_v_centi: i64,
    a_score: f64,
) -> bool {
    if b_entry.death_save_use_count != a_entry.death_save_use_count {
        return b_entry.death_save_use_count < a_entry.death_save_use_count;
    }
    if b_v_centi != a_v_centi {
        return b_v_centi > a_v_centi;
    }
    if b_entry.goal_credit != a_entry.goal_credit {
        return b_entry.goal_credit > a_entry.goal_credit;
    }
    if b_entry.goal_count != a_entry.goal_count {
        return b_entry.goal_count > a_entry.goal_count;
    }
    if b_entry.end_turn != a_entry.end_turn {
        return b_entry.end_turn < a_entry.end_turn;
    }
    b_score > a_score
}

#[inline(always)]
fn choose_better_pilot_entry(
    a_entry: &TurnRolloutPilotEntry,
    a_v_centi: i64,
    a_score: f64,
    b_entry: &TurnRolloutPilotEntry,
    b_v_centi: i64,
    b_score: f64,
) -> bool {
    let rank_a = a_entry.outcome_rank();
    let rank_b = b_entry.outcome_rank();
    if rank_a != rank_b {
        rank_b > rank_a
    } else if a_entry.won() && b_entry.won() {
        is_better_victory(b_entry, b_v_centi, b_score, a_entry, a_v_centi, a_score)
    } else {
        b_score > a_score
    }
}

struct PilotOutcomeMeta {
    pilot_entry: TurnRolloutPilotEntry,
    score: f64,
    v_centi: i64,
    won: bool,
    is_clean_perfect_victory: bool,
    final_fast: FastEvalSummary,
    auto_potions_used: u16,
    won_key: Option<(StateHash, i32)>,
}

#[allow(clippy::too_many_arguments)]
fn run_pilot_from_base(
    ws: &mut RolloutWorkspace,
    use_slab_b: bool,
    content: &dyn ContentDispatcher,
    root_baseline: &RootEvalBaseline,
    brightest_flame_limit: Option<i32>,
    profile: &SolverSearchProfile,
    eval_mode: EvalMode,
    horizon_turns: usize,
    optional_paid_potion_cost: i32,
    optional_paid_potion_count: usize,
    parent_action_count: u16,
    has_unused_paid_potions: bool,
    pilot: PilotKind,
    compute_won_key: bool,
) -> Option<PilotOutcomeMeta> {
    let base_state = &ws.base_state;
    clone_battle_state_into(base_state, &mut ws.pilot_state);
    let (steps_out, choices_out) = if use_slab_b {
        (&mut ws.steps_b, &mut ws.choices_b)
    } else {
        (&mut ws.steps_a, &mut ws.choices_a)
    };
    let (total_steps, auto_potions_used) = run_pilot_turns::<true, _>(
        &mut ws.pilot_state,
        steps_out,
        choices_out,
        &mut ws.choice_scored_buf,
        &mut ws.sel_buf,
        &mut ws.intent_cache,
        |s| {
            clone_battle_state_into(base_state, s);
            true
        },
        content,
        root_baseline,
        brightest_flame_limit,
        pilot,
        horizon_turns,
        &mut ws.blocked_buf,
        &mut ws.empty_q,
        &mut ws.playable_buf,
        &mut ws.hand_buf,
        &mut ws.seen_keys,
        None,
    )?;

    let (mut final_fast, final_full) = if !ws.pilot_state.is_in_progress
        || ws.pilot_state.won
        || ws.pilot_state.battleworn_dummy_timed_out
        || !ws.pilot_state.has_living_player_creature()
    {
        (
            evaluate_fast(&ws.pilot_state, content, root_baseline, None),
            crate::eval::FullEvalDetails::default(),
        )
    } else {
        let (f, full_det, _) = evaluate_fast_and_full_fused(
            &ws.pilot_state,
            content,
            root_baseline,
            0,
            0,
            None,
            Some(&mut ws.intent_cache),
        );
        if f.won || f.player_dead || f.projected_player_hp <= 0 || f.is_event_defeat {
            (f, crate::eval::FullEvalDetails::default())
        } else {
            (f, full_det)
        }
    };
    let ds_count = final_fast
        .death_save_use_count
        .max(i32::from(ws.pilot_state.player.death_save_use_count));
    let ds_hp = final_fast.death_save_hp_restored.max(
        ws.pilot_state
            .player
            .death_save_relic_hp_restored
            .saturating_add(ws.pilot_state.player.death_save_potion_hp_restored),
    );
    final_fast.death_save_use_count = ds_count;
    final_fast.death_save_hp_restored = ds_hp;
    let has_any_strategy_targets =
        root_baseline.effective_has_growth_targets || !root_baseline.active_relic_targets.is_empty();
    let all_growth_sat = !root_baseline.effective_has_growth_targets
        || root_baseline
            .growth_opportunity_targets
            .is_satisfied_by(&final_fast.growth_rewards);
    let all_relic_sat = root_baseline.active_relic_targets.is_empty()
        || root_baseline.relic_targets_satisfied(&final_fast.relic_eval);
    let all_strategy_targets_satisfied =
        final_fast.all_strategy_targets_satisfied || (all_growth_sat && all_relic_sat);
    final_fast.all_strategy_targets_satisfied = all_strategy_targets_satisfied;

    let pilot_entry = TurnRolloutPilotEntry::from_rollout_state(
        &ws.pilot_state,
        &final_fast,
        &final_full,
        root_baseline,
        profile,
        total_steps,
    );
    let (score, v_centi) = pilot_entry.materialize_outcome(
        root_baseline,
        eval_mode,
        optional_paid_potion_cost,
        optional_paid_potion_count,
        parent_action_count,
        has_unused_paid_potions,
    );
    let is_real_victory = pilot_entry.won();
    let is_clean_perfect_victory = is_real_victory
        && ds_count == 0
        && final_fast.cumulative_hp_lost == 0
        && final_fast.player_hp >= root_baseline.initial_player_max_hp
        && !root_baseline.has_any_heal_or_unknown_generation
        && !root_baseline.has_alchemize_in_deck
        && (!has_any_strategy_targets || all_strategy_targets_satisfied)
        && (!compute_won_key || !steps_out.is_empty());
    let won_key = if compute_won_key && is_real_victory {
        Some((
            hash_canonical_key(&ws.pilot_state),
            ws.pilot_state.player.turn_number,
        ))
    } else {
        None
    };
    Some(PilotOutcomeMeta {
        pilot_entry,
        score,
        v_centi,
        won: is_real_victory,
        is_clean_perfect_victory,
        final_fast,
        auto_potions_used,
        won_key,
    })
}

#[allow(clippy::too_many_arguments)]
fn evaluate_single_candidate(
    ws: &mut RolloutWorkspace,
    pid: NodeId,
    state_arena: &MainStateArena,
    arena: &NodeArena,
    root_ctx: &Arc<RootStaticContext>,
    content: &dyn ContentDispatcher,
    root_baseline: &RootEvalBaseline,
    policy: &SearchPolicy,
    profile: &SolverSearchProfile,
    eval_mode: EvalMode,
    horizon_turns: usize,
    rollout_cache: Option<&TurnRolloutCache>,
) -> Option<RolloutResult> {
    let pnode = arena.get(pid);
    if pnode.is_terminal() || pnode.state_handle.len == 0 {
        return None;
    }
    let can_promote = parent_satisfies_hard_potions(arena, pid, root_baseline, policy);
    let opt_cost = pnode.optional_paid_potion_cost as i32;
    let opt_count = pnode.optional_paid_potion_count as usize;
    let parent_act = pnode.action_count;
    let pnode_hash = pnode.state_hash;
    let has_any_strategy_targets =
        root_baseline.effective_has_growth_targets || !root_baseline.active_relic_targets.is_empty();
    let cfg_hash = turn_rollout_cfg_hash_with_policy(
        profile,
        horizon_turns,
        policy.brightest_flame_max_hp_loss_limit,
        has_any_strategy_targets,
    );
    let has_unused_paid_potions = root_baseline.effective_potion_policy
        != SolverPotionPolicy::Disabled
        && root_baseline.paid_optional_potion_count > 0
        && (root_baseline.paid_optional_slot_mask & !pnode.used_potion_slot_mask) != 0;

    if let Some(cache) = rollout_cache {
        if let Some(cached) = cache.get(pnode_hash, cfg_hash) {
            let Some(entry) = cached else {
                return None;
            };
            let any_won = entry.a.won() || entry.b.won();
            if !(can_promote && any_won) {
                let out_a = entry.a.is_valid().then(|| {
                    (
                        entry.a,
                        entry.a.materialize_outcome(
                            root_baseline,
                            eval_mode,
                            opt_cost,
                            opt_count,
                            parent_act,
                            has_unused_paid_potions,
                        ),
                    )
                });
                let out_b = entry.b.is_valid().then(|| {
                    (
                        entry.b,
                        entry.b.materialize_outcome(
                            root_baseline,
                            eval_mode,
                            opt_cost,
                            opt_count,
                            parent_act,
                            has_unused_paid_potions,
                        ),
                    )
                });
                let chosen_score = match (out_a, out_b) {
                    (Some((ae, (as_, av))), Some((be, (bs, bv)))) => {
                        if choose_better_pilot_entry(&ae, av, as_, &be, bv, bs) {
                            bs
                        } else {
                            as_
                        }
                    }
                    (Some((_, (as_, _))), None) => as_,
                    (None, Some((_, (bs, _)))) => bs,
                    (None, None) => return None,
                };
                return Some(RolloutResult {
                    rollout_score: chosen_score,
                    promoted_victory: None,
                });
            }
        }
    }

    let pbytes = state_arena.get(pnode.state_handle);
    if unsafe { unpack_state(pbytes, root_ctx, &mut ws.base_state) }.is_err() {
        return None;
    }
    #[cfg(debug_assertions)]
    debug_assert_eq!(pnode_hash, hash_canonical_key(&ws.base_state));
    if !ws.base_state.is_in_progress
        || ws.base_state.won
        || ws.base_state.battleworn_dummy_timed_out
        || ws.base_state.pending.is_some()
        || !ws.base_state.has_living_player_creature()
    {
        if let Some(cache) = rollout_cache {
            cache.insert(pnode_hash, cfg_hash, None);
        }
        return None;
    }

    let brightest_flame_limit = policy.brightest_flame_max_hp_loss_limit;
    let res_a = run_pilot_from_base(
        ws,
        false,
        content,
        root_baseline,
        brightest_flame_limit,
        profile,
        eval_mode,
        horizon_turns,
        opt_cost,
        opt_count,
        parent_act,
        has_unused_paid_potions,
        PilotKind::DefensiveTempo,
        can_promote,
    );
    let skip_pilot_b = res_a.as_ref().is_some_and(|a| {
        a.is_clean_perfect_victory
            || (!has_any_strategy_targets && !has_setup_first_relevant_cards(&ws.base_state, content))
    });
    let res_b = if skip_pilot_b {
        None
    } else {
        run_pilot_from_base(
            ws,
            true,
            content,
            root_baseline,
            brightest_flame_limit,
            profile,
            eval_mode,
            horizon_turns,
            opt_cost,
            opt_count,
            parent_act,
            has_unused_paid_potions,
            PilotKind::SetupFirst,
            can_promote,
        )
    };

    if let Some(cache) = rollout_cache {
        let entry_opt = if res_a.is_none() && res_b.is_none() {
            None
        } else {
            Some(TurnRolloutEntry {
                a: res_a
                    .as_ref()
                    .map_or(TurnRolloutPilotEntry::EMPTY, |r| r.pilot_entry),
                b: res_b
                    .as_ref()
                    .map_or(TurnRolloutPilotEntry::EMPTY, |r| r.pilot_entry),
            })
        };
        cache.insert(pnode_hash, cfg_hash, entry_opt);
    }

    let winner_is_b = match (&res_a, &res_b) {
        (Some(a), Some(b)) => choose_better_pilot_entry(
            &a.pilot_entry,
            a.v_centi,
            a.score,
            &b.pilot_entry,
            b.v_centi,
            b.score,
        ),
        (Some(_), None) => false,
        (None, Some(_)) => true,
        (None, None) => return None,
    };

    let chosen_score = if winner_is_b {
        res_b.as_ref().unwrap().score
    } else {
        res_a.as_ref().unwrap().score
    };
    let a_won = res_a.as_ref().is_some_and(|a| a.won);
    let b_won = res_b.as_ref().is_some_and(|b| b.won);

    let promoted_victory = if can_promote && (a_won || b_won) {
        let mut promote_b = if b_won && a_won {
            winner_is_b
        } else {
            b_won
        };
        if promote_b && ws.steps_b.is_empty() && a_won && !ws.steps_a.is_empty() {
            promote_b = false;
        } else if !promote_b && ws.steps_a.is_empty() && b_won && !ws.steps_b.is_empty() {
            promote_b = true;
        }
        let (win_steps, win_choices, promo_meta) = if promote_b {
            (&ws.steps_b, &ws.choices_b, res_b.unwrap())
        } else {
            (&ws.steps_a, &ws.choices_a, res_a.unwrap())
        };
        if !win_steps.is_empty() {
            let (final_state_hash, final_turn) = promo_meta.won_key?;
            let choices = if win_choices.steps_meta.is_empty() {
                RolloutChoiceSlab::default()
            } else {
                win_choices.clone()
            };
            Some(PromotedVictory {
                steps: SmallVec::from_slice(win_steps),
                choices,
                final_fast: promo_meta.final_fast,
                final_turn,
                final_state_hash,
                auto_potions_used: promo_meta.auto_potions_used,
            })
        } else {
            None
        }
    } else {
        None
    };

    Some(RolloutResult {
        rollout_score: chosen_score,
        promoted_victory,
    })
}

#[allow(clippy::too_many_arguments)]
pub fn evaluate_turn_boundary_rollouts(
    state_arena: &MainStateArena,
    arena: &NodeArena,
    shortlist: &[NodeId],
    root_ctx: &Arc<RootStaticContext>,
    content: &(dyn ContentDispatcher + Sync),
    root_baseline: &RootEvalBaseline,
    policy: &SearchPolicy,
    profile: &SolverSearchProfile,
    eval_mode: EvalMode,
    horizon_turns: usize,
    deadline: Option<Instant>,
    pool: Option<&rayon::ThreadPool>,
    cancel: &AtomicBool,
) -> Vec<Option<RolloutResult>> {
    let m = shortlist.len();
    if m == 0 || horizon_turns == 0 {
        return vec![None; m];
    }
    let dop = policy
        .max_degree_of_parallelism
        .clamp(1, crate::eval::weights::MAXIMUM_SEARCH_MAX_DEGREE_OF_PARALLELISM);
    let workers = if dop <= 1 || pool.is_none() || m < 4 {
        1
    } else {
        dop.min(m)
    };
    let mut workspaces: Vec<RolloutWorkspace> = (0..workers)
        .map(|_| RolloutWorkspace::new(root_ctx.clone()))
        .collect();
    evaluate_turn_boundary_rollouts_with_workspaces(
        state_arena,
        arena,
        shortlist,
        root_ctx,
        content,
        root_baseline,
        policy,
        profile,
        eval_mode,
        horizon_turns,
        deadline,
        pool,
        cancel,
        &mut workspaces,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn evaluate_turn_boundary_rollouts_with_workspaces(
    state_arena: &MainStateArena,
    arena: &NodeArena,
    shortlist: &[NodeId],
    root_ctx: &Arc<RootStaticContext>,
    content: &(dyn ContentDispatcher + Sync),
    root_baseline: &RootEvalBaseline,
    policy: &SearchPolicy,
    profile: &SolverSearchProfile,
    eval_mode: EvalMode,
    horizon_turns: usize,
    deadline: Option<Instant>,
    pool: Option<&rayon::ThreadPool>,
    cancel: &AtomicBool,
    workspaces: &mut [RolloutWorkspace],
    rollout_cache: Option<&TurnRolloutCache>,
) -> Vec<Option<RolloutResult>> {
    let mut out = Vec::with_capacity(shortlist.len());
    evaluate_turn_boundary_rollouts_into(
        state_arena,
        arena,
        shortlist,
        root_ctx,
        content,
        root_baseline,
        policy,
        profile,
        eval_mode,
        horizon_turns,
        deadline,
        pool,
        cancel,
        workspaces,
        rollout_cache,
        &mut out,
    );
    out
}

/// Evaluates turn-boundary rollout candidates sequentially or via the shared Rayon pool.
///
/// Parallel (`DOP=N`) vs single-thread (`DOP=1`) execution produces bit-identical results whenever
/// `deadline` is `None` or not reached; when a wall-clock `deadline` expires mid-batch, worker count
/// and OS scheduling decide how many candidates complete before cancellation (matching C# `HotSearchCoordinator`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn evaluate_turn_boundary_rollouts_into(
    state_arena: &MainStateArena,
    arena: &NodeArena,
    shortlist: &[NodeId],
    root_ctx: &Arc<RootStaticContext>,
    content: &(dyn ContentDispatcher + Sync),
    root_baseline: &RootEvalBaseline,
    policy: &SearchPolicy,
    profile: &SolverSearchProfile,
    eval_mode: EvalMode,
    horizon_turns: usize,
    deadline: Option<Instant>,
    pool: Option<&rayon::ThreadPool>,
    cancel: &AtomicBool,
    workspaces: &mut [RolloutWorkspace],
    rollout_cache: Option<&TurnRolloutCache>,
    out: &mut Vec<Option<RolloutResult>>,
) {
    let m = shortlist.len();
    out.clear();
    if m == 0 || horizon_turns == 0 || workspaces.is_empty() {
        out.resize_with(m, || None);
        return;
    }

    let dop = policy
        .max_degree_of_parallelism
        .clamp(1, crate::eval::weights::MAXIMUM_SEARCH_MAX_DEGREE_OF_PARALLELISM);
    if dop <= 1 || pool.is_none() || m < 4 || workspaces.len() < 2 {
        let ws = &mut workspaces[0];
        out.reserve(m);
        for &pid in shortlist {
            if cancel.load(Ordering::Relaxed) || deadline.is_some_and(|d| Instant::now() >= d) {
                out.push(None);
                continue;
            }
            out.push(evaluate_single_candidate(
                ws,
                pid,
                state_arena,
                arena,
                root_ctx,
                content,
                root_baseline,
                policy,
                profile,
                eval_mode,
                horizon_turns,
                rollout_cache,
            ));
        }
        return;
    }

    let pool = pool.unwrap();
    let workers = dop.min(m).min(workspaces.len());
    let batch_size = (m / (workers * 4)).clamp(1, 4);
    out.resize_with(m, || None);
    let next_idx = std::sync::atomic::AtomicUsize::new(0);
    let next_ref = &next_idx;
    let run_worker = |ws: &mut RolloutWorkspace| loop {
        if cancel.load(Ordering::Relaxed)
            || deadline.is_some_and(|d| Instant::now() >= d)
        {
            break;
        }
        let start = next_ref.fetch_add(batch_size, Ordering::Relaxed);
        if start >= m {
            break;
        }
        let end = (start + batch_size).min(m);
        for idx in start..end {
            if cancel.load(Ordering::Relaxed)
                || deadline.is_some_and(|d| Instant::now() >= d)
            {
                break;
            }
            let r = evaluate_single_candidate(
                ws,
                shortlist[idx],
                state_arena,
                arena,
                root_ctx,
                content,
                root_baseline,
                policy,
                profile,
                eval_mode,
                horizon_turns,
                rollout_cache,
            );
            ws.results_buf.push((idx, r));
        }
    };
    let worker_fn = &run_worker;

    let (first_ws, rest_ws) = workspaces[..workers]
        .split_first_mut()
        .expect("workers >= 2");
    first_ws.results_buf.clear();
    for ws in rest_ws.iter_mut() {
        ws.results_buf.clear();
    }

    pool.scope(|scope| {
        for ws in rest_ws.iter_mut() {
            scope.spawn(move |_| worker_fn(ws));
        }
        worker_fn(first_ws);
    });

    for ws in workspaces[..workers].iter_mut() {
        for (idx, res) in ws.results_buf.drain(..) {
            out[idx] = res;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::{BeamWeightPerturbation, BossHpRelief, RelicCounterEvaluation};

    #[test]
    fn choice_probe_cache_lazy_shards_and_four_way_eviction() {
        let cache = ChoiceProbeCache::new();
        assert!(cache.shards.iter().all(|s| s.table.get().is_none()));

        let h0 = StateHash(0x0000_0000_0001_0001u128);
        let cfg_a = choice_probe_cfg_hash(&SolverSearchProfile::default(), 2);
        let cfg_b = choice_probe_cfg_hash(&SolverSearchProfile::default(), 3);
        assert_ne!(cfg_a, cfg_b);

        let r0 = CachedProbeEntry {
            scores_at_zero: [123.5, 223.5, 323.5, 423.5],
            hp_lost: 4,
            setup_score: 9,
            board_investment: 17,
            strategy_goal_hp_credit: 3,
            strategic_hp_credit: 5,
            alchemize_count: 1,
            empty_potion_slots: 2,
            is_victory: true,
            is_defeat: false,
            is_event_defeat: false,
            linear_adj: true,
        };
        let r1 = CachedProbeEntry {
            scores_at_zero: [-45.25, -45.25, -45.25, -45.25],
            hp_lost: 12,
            setup_score: 0,
            board_investment: -3,
            strategy_goal_hp_credit: 0,
            strategic_hp_credit: 0,
            alchemize_count: 0,
            empty_potion_slots: 1,
            is_victory: false,
            is_defeat: true,
            is_event_defeat: false,
            linear_adj: false,
        };

        let (sh0, set0) = ChoiceProbeCache::slot_indices(h0, cfg_a);
        let same_set_hashes: Vec<StateHash> = (1u128..1_000_000u128)
            .filter_map(|k| {
                let candidate = StateHash((k << 64) ^ (h0.0 & 0xFFFF_FFFF_FFFF_FFFF));
                if ChoiceProbeCache::slot_indices(candidate, cfg_a) == (sh0, set0)
                    && candidate != h0
                {
                    Some(candidate)
                } else {
                    None
                }
            })
            .take(4)
            .collect();
        assert_eq!(same_set_hashes.len(), 4);
        let h_a = h0;
        let h_b = same_set_hashes[0];
        let h_c = same_set_hashes[1];
        let h_d = same_set_hashes[2];
        let h_e = same_set_hashes[3];

        cache.insert(h_a, cfg_a, Some(r0));
        assert_eq!(
            cache
                .shards
                .iter()
                .filter(|s| s.table.get().is_some())
                .count(),
            1
        );

        cache.insert(h_b, cfg_a, Some(r1));
        cache.insert(h_c, cfg_a, Some(r0));
        cache.insert(h_d, cfg_a, Some(r1));
        cache.insert(h_a, cfg_b, None);

        let got_a = cache.get(h_a, cfg_a).flatten().unwrap();
        assert_eq!(got_a.scores_at_zero[0].to_bits(), r0.scores_at_zero[0].to_bits());
        assert_eq!(got_a.scores_at_zero[1].to_bits(), r0.scores_at_zero[1].to_bits());
        assert_eq!(got_a.scores_at_zero[2].to_bits(), r0.scores_at_zero[2].to_bits());
        assert_eq!(got_a.scores_at_zero[3].to_bits(), r0.scores_at_zero[3].to_bits());
        assert_eq!(got_a.hp_lost, r0.hp_lost);
        assert_eq!(got_a.setup_score, r0.setup_score);
        assert_eq!(got_a.board_investment, r0.board_investment);
        assert_eq!(got_a.strategy_goal_hp_credit, r0.strategy_goal_hp_credit);
        assert_eq!(got_a.strategic_hp_credit, r0.strategic_hp_credit);
        assert_eq!(got_a.alchemize_count, r0.alchemize_count);
        assert_eq!(got_a.empty_potion_slots, r0.empty_potion_slots);
        assert_eq!(got_a.is_victory, r0.is_victory);
        assert_eq!(got_a.is_defeat, r0.is_defeat);
        assert_eq!(got_a.is_event_defeat, r0.is_event_defeat);
        assert_eq!(got_a.linear_adj, r0.linear_adj);

        assert!(cache.get(h_b, cfg_a).is_some());
        assert!(cache.get(h_c, cfg_a).is_some());
        assert!(cache.get(h_d, cfg_a).is_some());
        assert!(matches!(cache.get(h_a, cfg_b), Some(None)));

        cache.insert(h_e, cfg_a, Some(r0));
        assert!(cache.get(h_e, cfg_a).is_some());
        assert!(cache.get(h_a, cfg_a).is_none());
        assert!(cache.get(h_b, cfg_a).is_some());

        cache.clear();
        assert!(cache.get(h_e, cfg_a).is_none());
        assert!(cache.get(h_a, cfg_b).is_none());
    }

    #[test]
    fn choice_probe_and_turn_rollout_seqlock_concurrent_stress() {
        let probe_cache = ChoiceProbeCache::new();
        let rollout_cache = TurnRolloutCache::new();
        let cfg_p = choice_probe_cfg_hash(&SolverSearchProfile::default(), 2);
        let cfg_r = turn_rollout_cfg_hash(&SolverSearchProfile::default(), 3);

        std::thread::scope(|s| {
            for tid in 0..8usize {
                let pc = &probe_cache;
                let rc = &rollout_cache;
                s.spawn(move || {
                    for iter in 0..400u64 {
                        let key_id = (iter % 12) + 1;
                        let h = StateHash::from_parts(key_id, (tid as u64) & 1);
                        let p_entry = CachedProbeEntry {
                            scores_at_zero: [
                                (key_id as f64) * 1.5,
                                (key_id as f64) * 2.5,
                                (key_id as f64) * 3.5,
                                (key_id as f64) * 4.5,
                            ],
                            hp_lost: key_id as i32,
                            setup_score: 0,
                            board_investment: (key_id as i32) * 3,
                            strategy_goal_hp_credit: 0,
                            strategic_hp_credit: 0,
                            alchemize_count: (key_id & 3) as u8,
                            empty_potion_slots: ((key_id + 1) & 3) as u8,
                            is_victory: (key_id & 1) == 0,
                            is_defeat: false,
                            is_event_defeat: false,
                            linear_adj: true,
                        };
                        pc.insert(h, cfg_p, Some(p_entry));
                        if let Some(Some(got)) = pc.get(h, cfg_p) {
                            assert_eq!(got.scores_at_zero[0].to_bits(), p_entry.scores_at_zero[0].to_bits());
                            assert_eq!(got.hp_lost, p_entry.hp_lost);
                            assert_eq!(got.board_investment, p_entry.board_investment);
                            assert_eq!(got.alchemize_count, p_entry.alchemize_count);
                            assert_eq!(got.empty_potion_slots, p_entry.empty_potion_slots);
                            assert_eq!(got.is_victory, p_entry.is_victory);
                        }

                        let r_entry = TurnRolloutEntry {
                            a: TurnRolloutPilotEntry {
                                scores_at_zero: [
                                    (key_id as f64) * 4.0,
                                    (key_id as f64) * 5.0,
                                    (key_id as f64) * 6.0,
                                    (key_id as f64) * 6.5,
                                ],
                                goal_credit: key_id as i32,
                                goal_count: 1,
                                end_turn: 2,
                                death_save_use_count: 0,
                                alchemize_count: (key_id & 7) as u8,
                                empty_potion_slots: 2,
                                flags: ROLLOUT_PILOT_VALID | ROLLOUT_PILOT_LINEAR_ADJ,
                            },
                            b: TurnRolloutPilotEntry {
                                scores_at_zero: [
                                    (key_id as f64) * 7.0,
                                    (key_id as f64) * 8.0,
                                    (key_id as f64) * 9.0,
                                    (key_id as f64) * 9.5,
                                ],
                                goal_credit: (key_id as i32) * 2,
                                goal_count: 2,
                                end_turn: 3,
                                death_save_use_count: 1,
                                alchemize_count: 1,
                                empty_potion_slots: 1,
                                flags: ROLLOUT_PILOT_VALID | ROLLOUT_PILOT_WON,
                            },
                        };
                        rc.insert(h, cfg_r, Some(r_entry));
                        if let Some(Some(got)) = rc.get(h, cfg_r) {
                            assert_eq!(got.a.scores_at_zero[0].to_bits(), r_entry.a.scores_at_zero[0].to_bits());
                            assert_eq!(got.b.scores_at_zero[2].to_bits(), r_entry.b.scores_at_zero[2].to_bits());
                            assert_eq!(got.a.alchemize_count, r_entry.a.alchemize_count);
                            assert_eq!(got.b.flags, r_entry.b.flags);
                        }
                    }
                });
            }
        });
    }

    #[test]
    fn turn_rollout_cache_cross_mode_and_linear_materialization() {
        let root = RootEvalBaseline {
            initial_player_hp: 70,
            initial_player_max_hp: 80,
            initial_enemy_count: 2,
            start_turn_number: 1,
            boss_hp_relief: BossHpRelief::ActClearHeal,
            belt_full_at_root: true,
            base_replacement_credit_centi: 900,
            ..Default::default()
        };
        let fast_ongoing = FastEvalSummary {
            base_score: 12_345.5,
            player_hp: 55,
            player_max_hp: 80,
            projected_player_hp: 40,
            cumulative_hp_lost: 15,
            energy: 2,
            stars: 1,
            alchemize_count: 1,
            empty_potion_slots: 2,
            enemy_hp: 88,
            alive_enemy_count: 2,
            ..Default::default()
        };
        let full_ongoing = FullEvalDetails {
            board_investment: 30,
            unblocked_incoming: 10,
            persistent_buff_value: 20,
            playable_card_count: 3,
            ..Default::default()
        };
        let profile = SolverSearchProfile::default();
        let entry_a = TurnRolloutPilotEntry::from_eval(&fast_ongoing, &full_ongoing, &root, &profile, 6);
        let cache = TurnRolloutCache::new();
        let hash = StateHash(0x1234_5678_9ABC_DEF0u128);
        let cfg = turn_rollout_cfg_hash(&profile, 4);
        cache.insert(
            hash,
            cfg,
            Some(TurnRolloutEntry {
                a: entry_a,
                b: TurnRolloutPilotEntry::EMPTY,
            }),
        );
        let hit = cache.get(hash, cfg).flatten().unwrap();
        for &mode in &[
            EvalMode::Balanced,
            EvalMode::SetupAndSurvival,
            EvalMode::Offense,
            EvalMode::DesperateRescue,
        ] {
            for &(opt_cost, opt_cnt, parent_act) in &[(0, 0, 0), (9, 1, 5), (18, 2, 11)] {
                let got = hit.a.materialize_score(&root, mode, opt_cost, opt_cnt, parent_act);
                let net_c = PotionUsePolicy::compute_net_potion_opportunity_cost_centi(
                    opt_cost,
                    opt_cnt as i32,
                    fast_ongoing.alchemize_count as i32,
                    fast_ongoing.empty_potion_slots as i32,
                    &root,
                );
                let direct = compute_beam_rank_score_with_centi(
                    &fast_ongoing,
                    &full_ongoing,
                    &root,
                    profile.base_score_only,
                    profile.boss_tempo_hp_pricing,
                    profile.beam_weight_perturbation,
                    mode,
                    net_c,
                    parent_act + 6,
                );
                assert!((got - direct).abs() <= 1e-9, "turn rollout mismatch: got={got} direct={direct}");
            }
        }
        cache.clear();
        assert!(cache.get(hash, cfg).is_none());
    }

    #[test]
    fn linear_probe_materialization_matches_direct_rank_score() {
        let root = RootEvalBaseline {
            initial_player_hp: 70,
            initial_player_max_hp: 80,
            initial_enemy_count: 2,
            start_turn_number: 1,
            boss_hp_relief: BossHpRelief::ActClearHeal,
            belt_full_at_root: true,
            base_replacement_credit_centi: 900,
            ..Default::default()
        };
        let fast_ongoing = FastEvalSummary {
            base_score: 123_456.5,
            player_hp: 58,
            player_max_hp: 80,
            projected_player_hp: 45,
            cumulative_hp_lost: 12,
            death_save_hp_restored: 0,
            strategic_hp_credit: 200,
            death_save_use_count: 0,
            combat_ended_turn: None,
            growth_reward_count: 2,
            relic_eval: RelicCounterEvaluation {
                satisfied_priority: 3,
                distance: 1,
                ..Default::default()
            },
            outstanding_stolen: 15,
            enemy_hp: 94,
            enemy_block: 6,
            alive_enemy_count: 2,
            reviving_enemy_count: 1,
            energy: 2,
            stars: 3,
            live_deck_clutter: 1,
            alchemize_count: 1,
            empty_potion_slots: 2,
            won: false,
            player_dead: false,
            forfeits_gold: true,
            ..Default::default()
        };
        let full_ongoing = FullEvalDetails {
            retained_blade_bonus_base: 18,
            board_investment: 42,
            unblocked_incoming: 13,
            end_turn_block: 20,
            incoming_damage: 15,
            next_turn_block: 8,
            next_turn_energy: 2,
            next_turn_draw: 1,
            next_turn_stars: 2,
            next_turn_summon: 3,
            doom_lethal_count: 1,
            reviving_hp_penalty: 12,
            persistent_buff_value: 25,
            enemy_debuff_value: 9,
            enemy_threat_power_penalty: 4,
            orb_capacity: 3,
            orb_score: 21,
            osty_hp: 15,
            highest_alive_enemy_hp: 60,
            focus_target_remaining_hp: 34,
            min_sandpit_turns: 3,
            ready_arrows: 2,
            melody_cards_finished: 1,
            playable_card_count: 4,
            zero_cost_playable_count: 1,
            hand_clutter: 1,
            effective_doom_value: 17,
            all_reattach_encounter: false,
            preserves_excess_block: true,
            ..Default::default()
        };
        let mut fast_won = fast_ongoing;
        fast_won.won = true;
        fast_won.enemy_hp = 0;
        fast_won.alive_enemy_count = 0;
        fast_won.reviving_enemy_count = 0;
        fast_won.combat_ended_turn = Some(4);

        let mut fast_dead = fast_ongoing;
        fast_dead.player_dead = true;
        fast_dead.projected_player_hp = 0;

        let modes = [
            EvalMode::Balanced,
            EvalMode::SetupAndSurvival,
            EvalMode::Offense,
            EvalMode::DesperateRescue,
        ];
        let perts = [
            None,
            Some(BeamWeightPerturbation {
                term: BeamWeightTerm::CurrentEnergy,
                scale: 1.25,
            }),
            Some(BeamWeightPerturbation {
                term: BeamWeightTerm::PersistentBuffDelta,
                scale: 0.75,
            }),
            Some(BeamWeightPerturbation {
                term: BeamWeightTerm::EnemyHp,
                scale: 1.4,
            }),
        ];

        for (fast, full) in [
            (&fast_ongoing, &full_ongoing),
            (&fast_won, &FullEvalDetails::default()),
            (&fast_dead, &FullEvalDetails::default()),
        ] {
            let is_victory = fast.won;
            let is_defeat = fast.player_dead || fast.projected_player_hp <= 0;
            for &base_only in &[false, true] {
                for &pert in &perts {
                    let profile = SolverSearchProfile {
                        soft_time_budget_ms: 0,
                        base_score_only: base_only,
                        beam_weight_perturbation: pert,
                        ..Default::default()
                    };
                    let probe_steps = 7u16;
                    let linear_adj = !(base_only || is_defeat);
                    let scores_at_zero = compute_scores_at_zero(
                        fast,
                        full,
                        &root,
                        &profile,
                        linear_adj,
                        probe_steps,
                    );
                    let entry = CachedProbeEntry {
                        scores_at_zero,
                        hp_lost: fast.cumulative_hp_lost,
                        setup_score: full.setup_score,
                        board_investment: full.board_investment,
                        strategy_goal_hp_credit: fast.strategy_goal_hp_credit,
                        strategic_hp_credit: fast.strategic_hp_credit,
                        alchemize_count: fast.alchemize_count,
                        empty_potion_slots: fast.empty_potion_slots,
                        is_victory,
                        is_defeat,
                        is_event_defeat: fast.is_event_defeat,
                        linear_adj,
                    };
                    for &eval_mode in &modes {
                        for &(opt_cost, opt_cnt, p_act) in &[(0, 0, 0), (9, 1, 5), (18, 2, 12)] {
                            let mat = entry.materialize_result(
                                &root,
                                eval_mode,
                                opt_cost,
                                opt_cnt,
                                p_act,
                            );
                            let net_c = PotionUsePolicy::compute_net_potion_opportunity_cost_centi(
                                opt_cost,
                                opt_cnt as i32,
                                fast.alchemize_count as i32,
                                fast.empty_potion_slots as i32,
                                &root,
                            );
                            let direct = compute_beam_rank_score_with_centi(
                                fast,
                                full,
                                &root,
                                profile.base_score_only,
                                profile.boss_tempo_hp_pricing,
                                profile.beam_weight_perturbation,
                                eval_mode,
                                net_c,
                                p_act.saturating_add(probe_steps),
                            );
                            assert!(
                                (mat.score - direct).abs() <= 1e-9,
                                "linear mismatch: mat={} direct={}",
                                mat.score,
                                direct
                            );
                            assert_eq!(mat.hp_lost, fast.cumulative_hp_lost);
                            assert_eq!(mat.board_investment, full.board_investment);
                            assert_eq!(mat.is_victory, fast.won);
                            assert_eq!(mat.is_defeat, is_defeat);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn rollout_dedup_key_fast_u64_round_trip() {
        let card_id = CardId::StrikeIronclad;
        let samples = [(-1, -1), (0, -1), (2, 0), (3, 5)];
        for &(energy, star) in &samples {
            for &upg in &[0i32, 1i32] {
                for &has_ext in &[false, true] {
                    for aff_id in [None, Some(sts2_types::AfflictionId::Galvanized)] {
                        let expected =
                            RolloutDedupKey::new(card_id, upg, energy, star, has_ext, aff_id);
                        let fk = RolloutDedupKey::try_fast_u64(
                            card_id, upg, energy, star, has_ext, aff_id,
                        )
                        .expect("sample fits fast u64 dedup key");
                        assert_eq!(RolloutDedupKey::from_fast_u64(fk), expected);
                    }
                }
            }
        }
    }

    fn build_test_combat(
        character: sts2_types::CharacterId,
        deck_ids: &[CardId],
        relics: &[RelicId],
        enemies: &[(sts2_types::MonsterId, i32)],
    ) -> BattleState {
        use std::borrow::Cow;
        use sts2_content::Sts2Content;
        use sts2_kernel::{
            ArrowSlotInst, CardInst, CombatEngine, CreatureInst, MonsterInst, PlayerCounters,
            PlayerInst, RelicInst, SemanticHistory,
        };
        use sts2_types::{ArrowStatus, CombatSide, GameMode, PlayerRngSet, Rng, RoomType, RunRngSet};

        let content = Sts2Content;
        let run_rngs = RunRngSet::from_string_seed("ROLLOUT_UNIT_TEST");
        let player_rngs = PlayerRngSet::from_seed(run_rngs.seed);
        let pc_handle = CreatureHandle(1);

        let mut cards = Vec::new();
        let mut deck_handles = Vec::new();
        for (idx, &cid) in deck_ids.iter().enumerate() {
            let meta = content.card_meta(cid).unwrap();
            let h = CardHandle((idx + 1) as u32);
            deck_handles.push(h);
            cards.push(CardInst {
                handle: h,
                id: cid,
                upgrade_level: 0,
                canonical_base_energy_cost: meta.canonical_energy_cost as i16,
                base_energy_cost: meta.canonical_energy_cost as i16,
                captured_energy_x_value: 0,
                base_replay_count: 0,
                floor_added_to_deck: sts2_kernel::OptI16::some(1),
                deck_card_handle: sts2_kernel::OptCardHandle::some(h),
                clone_of_handle: sts2_kernel::OptCardHandle::NONE,
                temp_star_cost_override_this_turn: sts2_kernel::OptI16::NONE,
                temp_cost_override_this_turn: sts2_kernel::OptI16::NONE,
                temp_cost_override_this_turn_or_until_played: sts2_kernel::OptI16::NONE,
                temp_cost_override_this_combat: sts2_kernel::OptI16::NONE,
                local_keywords: meta.base_keywords,
                keywords_added_from_outside: CardKeywords::empty(),
                keywords_removed_from_outside: CardKeywords::empty(),
                temp_flags: CardTempFlags::empty(),
                affliction: None,
                ext_index: None,
                pile: PileType::Deck,
                removed_from_state: false,
                transform_into_after_play: None,
            });
        }

        let mut arrows = Vec::new();
        for _ in 0..character.base_arrow_slot_count() {
            arrows.push(ArrowSlotInst {
                status: ArrowStatus::Retrieved,
            });
        }

        let player_creature = CreatureInst {
            handle: pc_handle,
            combat_id: Some(0),
            side: CombatSide::Player,
            slot_name: None,
            is_player: true,
            pet_owner_player_index: None,
            monster_id: None,
            current_hp: character.starting_hp(),
            max_hp: character.starting_hp(),
            block: 0,
            cumulative_hp_lost: 0,
            is_detached: false,
            monster: None,
            has_minion_power: false,
            powers: Vec::new(),
        };

        let mut creatures = vec![player_creature];
        let mut enemy_handles = Vec::new();
        let mut encounter_slots = Vec::new();

        for (idx, &(enemy_id, enemy_hp)) in enemies.iter().enumerate() {
            let eh = CreatureHandle((idx + 2) as u32);
            let slot = format!("slot_{idx}");
            enemy_handles.push(eh);
            encounter_slots.push(slot.clone());
            let mon = MonsterInst {
                monster_id: enemy_id,
                rng: Rng::from_seed(run_rngs.seed + 1 + idx as u64),
                is_performing_move: false,
                spawned_this_turn: false,
                current_state_id: Cow::Borrowed(""),
                next_move_id: None,
                next_move_can_transition_away: true,
                performed_first_move: false,
                state_log_ids: Vec::new(),
                transient_move_follow_up_state_id: None,
                setup_fields: Default::default(),
                private_fields: Default::default(),
            };
            creatures.push(CreatureInst {
                handle: eh,
                combat_id: Some((idx + 1) as u32),
                side: CombatSide::Enemy,
                slot_name: Some(Cow::Owned(slot)),
                is_player: false,
                pet_owner_player_index: None,
                monster_id: Some(enemy_id),
                current_hp: enemy_hp,
                max_hp: enemy_hp,
                block: 0,
                cumulative_hp_lost: 0,
                is_detached: false,
                monster: Some(Box::new(mon)),
                has_minion_power: false,
                powers: Vec::new(),
            });
        }

        let player = PlayerInst {
            creature_handle: pc_handle,
            current_hp: character.starting_hp(),
            max_hp: character.starting_hp(),
            block: 0,
            cumulative_hp_lost: 0,
            gold: 99,
            max_energy: character.max_energy(),
            base_orb_slot_count: character.base_orb_slot_count(),
            is_active_for_hooks: true,
            can_use_or_remove_potions: true,
            energy: 0,
            stars: 0,
            phase: PlayerTurnPhase::None,
            turn_number: 1,
            counters: PlayerCounters::default(),
            hand: Vec::new(),
            draw: Vec::new(),
            discard: Vec::new(),
            exhaust: Vec::new(),
            play: Vec::new(),
            deck: deck_handles,
            potion_slots: smallvec::smallvec![None, None, None],
            combat_generated_potion_mask: 0,
            orb_capacity: character.base_orb_slot_count(),
            orbs: Vec::new(),
            arrows,
            relics: relics
                .iter()
                .map(|&id| RelicInst {
                    id,
                    is_wax: false,
                    is_melted: false,
                    stack_count: 1,
                    floor_added_to_deck: None,
                    private_fields: Default::default(),
                })
                .collect(),
            card_or_potion_effect_depth: 0,
            death_save_use_count: 0,
            death_save_relic_hp_restored: 0,
            death_save_potion_hp_restored: 0,
        };

        let spawned_enemy_handles = SmallVec::from_slice(&enemy_handles);
        let mut state = BattleState {
            static_ctx: Arc::new(RootStaticContext {
                encounter_id: "ROLLOUT_UNIT_TEST".to_owned(),
                encounter_is_weak: false,
                room_type: RoomType::Monster,
                encounter_slots,
                game_mode: GameMode::Standard,
                ascension: 0,
                character_id: character,
                dominion_id: None,
                run_model_ids: Vec::new(),
                card_generation_pools: Vec::new(),
                colorless_card_pool: Vec::new(),
                curse_card_pool: Vec::new(),
                other_character_card_pool: Vec::new(),
                tarot_card_pool: Vec::new(),
                potion_generation_pool: Vec::new(),
                transformation_pools: Vec::new(),
                map_col: 0,
                map_row: 0,
                act_index: 0,
                total_floor: 0,
                ..Default::default()
            }),
            current_side: CombatSide::Player,
            round_number: 1,
            is_in_progress: false,
            is_starting: false,
            won: false,
            is_player_extra_turn: false,
            gold_was_stolen: false,
            battleworn_dummy_timed_out: false,
            end_player_turn_requested: false,
            card_action_boundary_depth: 0,
            next_creature_id: (enemies.len() + 1) as u32,
            initial_encounter_monster_count: enemies.len(),
            next_card_handle: (cards.len() + 1) as u32,
            next_creature_handle: (enemies.len() + 2) as u32,
            next_power_handle: 1,
            next_attack_id: 1,
            run_rngs,
            player_rngs,
            player,
            cards,
            card_extensions: Vec::new(),
            creatures,
            power_extensions: Vec::new(),
            ally_handles: vec![pc_handle],
            enemy_handles,
            escaped_handles: SmallVec::new(),
            removed_handles: SmallVec::new(),
            spawned_enemy_handles,
            pet_handles: SmallVec::new(),
            attacking_creatures: SmallVec::new(),
            deferred_death_cleanup: SmallVec::new(),
            damage_history: Vec::new(),
            semantic_history: SemanticHistory::default(),
            pending: None,
            vakuu_auto_select_depth: 0,
            hook_cache: Default::default(),
            deck_dirty: Default::default(),
            relic_dirty_mask: Default::default(),
            handle_layout: Default::default(),
            card_meta_dirty: Default::default(),
            extra_clutter_hint: Default::default(),
            eot_card_cleanup_dirty: Default::default(),
        };
        CombatEngine::start_combat(&mut state, &content).unwrap();
        state
    }

    fn set_hand_and_draw(
        state: &mut BattleState,
        content: &sts2_content::Sts2Content,
        hand_cards: &[(CardId, i32)],
        draw_cards: &[(CardId, i32)],
    ) -> (Vec<CardHandle>, Vec<CardHandle>) {
        use sts2_kernel::CardInst;
        for c in &mut state.cards {
            c.removed_from_state = true;
            c.pile = PileType::None;
        }
        state.player.hand.clear();
        state.player.draw.clear();
        state.player.discard.clear();
        state.player.exhaust.clear();

        let mut push_card = |cid: CardId, upg: i32, pile: PileType| -> CardHandle {
            let mut h = CardHandle(state.next_card_handle);
            state.next_card_handle += 1;
            let meta = content.card_meta(cid).unwrap();
            let mut cost = meta.canonical_energy_cost as i16;
            if upg > 0 && upgrade_cost_reduction(cid, cost as i32) > 0 {
                cost = (cost as i32 - upgrade_cost_reduction(cid, cost as i32)).max(0) as i16;
            }
            let inst = CardInst {
                handle: h,
                id: cid,
                upgrade_level: upg,
                canonical_base_energy_cost: meta.canonical_energy_cost as i16,
                base_energy_cost: cost,
                captured_energy_x_value: 0,
                base_replay_count: 0,
                floor_added_to_deck: sts2_kernel::OptI16::some(1),
                deck_card_handle: sts2_kernel::OptCardHandle::some(h),
                clone_of_handle: sts2_kernel::OptCardHandle::NONE,
                temp_star_cost_override_this_turn: sts2_kernel::OptI16::NONE,
                temp_cost_override_this_turn: sts2_kernel::OptI16::NONE,
                temp_cost_override_this_turn_or_until_played: sts2_kernel::OptI16::NONE,
                temp_cost_override_this_combat: sts2_kernel::OptI16::NONE,
                local_keywords: meta.base_keywords,
                keywords_added_from_outside: CardKeywords::empty(),
                keywords_removed_from_outside: CardKeywords::empty(),
                temp_flags: CardTempFlags::empty(),
                affliction: None,
                ext_index: None,
                pile,
                removed_from_state: false,
                transform_into_after_play: None,
            };
            state.cards.push(inst);
            h = CardHandle(state.cards.len() as u32);
            state.cards.last_mut().unwrap().handle = h;
            state.next_card_handle = (state.cards.len() + 1) as u32;
            match pile {
                PileType::Hand => state.player.hand.push(h),
                PileType::Draw => state.player.draw.push(h),
                _ => {}
            }
            h
        };

        let mut hh = Vec::with_capacity(hand_cards.len());
        for &(cid, upg) in hand_cards {
            hh.push(push_card(cid, upg, PileType::Hand));
        }
        let mut dh = Vec::with_capacity(draw_cards.len());
        for &(cid, upg) in draw_cards {
            dh.push(push_card(cid, upg, PileType::Draw));
        }
        state.deck_dirty = sts2_kernel::DeckDirtyFlag(true);
        state.handle_layout.cards = false;
        state.card_meta_dirty = Default::default();
        state.hook_cache = Default::default();
        let _ = state.ensure_hook_cache(content);
        (hh, dh)
    }

    #[test]
    fn turn_rollout_bounded_terminal_and_desperate_rescue_scores() {
        let profile = SolverSearchProfile::default();

        // 1. Victory without strategy targets (+25M tier, death-save penalty -4M, floors 12M / 8M).
        let root_no_strat = RootEvalBaseline {
            initial_player_hp: 70,
            initial_player_max_hp: 80,
            start_turn_number: 1,
            ..Default::default()
        };
        let win_clean = FastEvalSummary {
            won: true,
            player_hp: 70,
            player_max_hp: 80,
            death_save_use_count: 0,
            alchemize_count: 0,
            empty_potion_slots: 1,
            ..Default::default()
        };
        let e_clean = TurnRolloutPilotEntry::from_rollout_fields(
            &win_clean,
            &FullEvalDetails::default(),
            &root_no_strat,
            &profile,
            4,
            2,
            false,
            0,
            0,
        );
        let (s_clean, vc_clean) =
            e_clean.materialize_outcome(&root_no_strat, EvalMode::Balanced, 0, 0, 0, false);
        assert_eq!(vc_clean, 0);
        assert_eq!(s_clean, (25_000_000 - 2 * 5_000) as f64);

        let e_ds = TurnRolloutPilotEntry::from_rollout_fields(
            &win_clean,
            &FullEvalDetails::default(),
            &root_no_strat,
            &profile,
            4,
            2,
            false,
            1,
            0,
        );
        let (s_ds, _) =
            e_ds.materialize_outcome(&root_no_strat, EvalMode::Balanced, 0, 0, 0, false);
        assert_eq!(s_ds, (25_000_000 - 2 * 5_000 - 4_000_000) as f64);

        // Floor test on severe HP loss.
        let root_floor = RootEvalBaseline {
            initial_player_hp: 400,
            initial_player_max_hp: 400,
            start_turn_number: 1,
            ..Default::default()
        };
        let win_battered = FastEvalSummary {
            won: true,
            player_hp: 1,
            player_max_hp: 400,
            ..Default::default()
        };
        let e_floor = TurnRolloutPilotEntry::from_rollout_fields(
            &win_battered,
            &FullEvalDetails::default(),
            &root_floor,
            &profile,
            10,
            20,
            false,
            0,
            0,
        );
        let (s_floor, _) =
            e_floor.materialize_outcome(&root_floor, EvalMode::Balanced, 0, 0, 0, false);
        assert_eq!(s_floor, 12_000_000.0);
        let e_floor_ds = TurnRolloutPilotEntry::from_rollout_fields(
            &win_battered,
            &FullEvalDetails::default(),
            &root_floor,
            &profile,
            10,
            20,
            false,
            1,
            0,
        );
        let (s_floor_ds, _) =
            e_floor_ds.materialize_outcome(&root_floor, EvalMode::Balanced, 0, 0, 0, false);
        assert_eq!(s_floor_ds, 8_000_000.0);

        // 2. Victory with active strategy targets: satisfied (25M), partial (16M), zero-progress (800k).
        let root_strat = RootEvalBaseline {
            initial_player_hp: 70,
            initial_player_max_hp: 80,
            start_turn_number: 1,
            effective_has_growth_targets: true,
            ..Default::default()
        };
        let win_sat = FastEvalSummary {
            won: true,
            player_hp: 70,
            player_max_hp: 80,
            all_strategy_targets_satisfied: true,
            strategy_goal_count: 2,
            strategy_goal_hp_credit: 6,
            strategic_hp_credit: 6,
            ..Default::default()
        };
        let win_part = FastEvalSummary {
            all_strategy_targets_satisfied: false,
            strategy_goal_count: 1,
            strategy_goal_hp_credit: 4,
            strategic_hp_credit: 4,
            ..win_sat
        };
        let win_zero = FastEvalSummary {
            all_strategy_targets_satisfied: false,
            strategy_goal_count: 0,
            strategy_goal_hp_credit: 0,
            strategic_hp_credit: 0,
            ..win_sat
        };
        let (s_sat, _) = TurnRolloutPilotEntry::from_rollout_fields(
            &win_sat,
            &FullEvalDetails::default(),
            &root_strat,
            &profile,
            2,
            2,
            false,
            0,
            0,
        )
        .materialize_outcome(&root_strat, EvalMode::Balanced, 0, 0, 0, false);
        let (s_part, _) = TurnRolloutPilotEntry::from_rollout_fields(
            &win_part,
            &FullEvalDetails::default(),
            &root_strat,
            &profile,
            2,
            2,
            false,
            0,
            0,
        )
        .materialize_outcome(&root_strat, EvalMode::Balanced, 0, 0, 0, false);
        let (s_zero, _) = TurnRolloutPilotEntry::from_rollout_fields(
            &win_zero,
            &FullEvalDetails::default(),
            &root_strat,
            &profile,
            2,
            2,
            false,
            0,
            0,
        )
        .materialize_outcome(&root_strat, EvalMode::Balanced, 0, 0, 0, false);
        assert!(s_sat > 25_000_000.0);
        assert!(s_part > 16_000_000.0 && s_part < 25_000_000.0);
        assert!(s_zero >= 200_000.0 && s_zero < 1_000_000.0);

        // 3. Defeat (-20M vs -12M in DesperateRescue or has_unused_paid_potions) + event defeat (+2M).
        let fast_defeat = FastEvalSummary {
            player_dead: true,
            projected_player_hp: 0,
            enemy_hp: 40,
            ..Default::default()
        };
        let e_defeat = TurnRolloutPilotEntry::from_rollout_fields(
            &fast_defeat,
            &FullEvalDetails::default(),
            &root_no_strat,
            &profile,
            3,
            3,
            false,
            0,
            0,
        );
        assert_eq!(e_defeat.outcome_rank(), 0);
        let (sd_bal, _) =
            e_defeat.materialize_outcome(&root_no_strat, EvalMode::Balanced, 0, 0, 0, false);
        let (sd_res, _) = e_defeat.materialize_outcome(
            &root_no_strat,
            EvalMode::DesperateRescue,
            0,
            0,
            0,
            false,
        );
        let (sd_pot, _) =
            e_defeat.materialize_outcome(&root_no_strat, EvalMode::Balanced, 0, 0, 0, true);
        let tail = 3 * 60_000 - 40 * 10_000;
        assert_eq!(sd_bal, (-20_000_000 + tail) as f64);
        assert_eq!(sd_res, (-12_000_000 + tail) as f64);
        assert_eq!(sd_pot, (-12_000_000 + tail) as f64);

        let fast_ev_defeat = FastEvalSummary {
            player_dead: false,
            projected_player_hp: 50,
            is_event_defeat: true,
            enemy_hp: 40,
            ..Default::default()
        };
        let e_ev = TurnRolloutPilotEntry::from_rollout_fields(
            &fast_ev_defeat,
            &FullEvalDetails::default(),
            &root_no_strat,
            &profile,
            3,
            3,
            false,
            0,
            0,
        );
        assert_eq!(e_ev.outcome_rank(), 1);
        let (sev_bal, _) =
            e_ev.materialize_outcome(&root_no_strat, EvalMode::Balanced, 0, 0, 0, false);
        let (sev_res, _) = e_ev.materialize_outcome(
            &root_no_strat,
            EvalMode::DesperateRescue,
            0,
            0,
            0,
            false,
        );
        assert_eq!(sev_bal, (-18_000_000 + tail) as f64);
        assert_eq!(sev_res, (-10_000_000 + tail) as f64);

        // Cache round-trip preserves goal_credit/goal_count/end_turn/death_save_use_count.
        let cache = TurnRolloutCache::new();
        let h = StateHash::from_parts(0xAABB_CCDD, 0x1122_3344);
        let cfg = turn_rollout_cfg_hash_with_policy(&profile, 4, Some(4), true);
        cache.insert(
            h,
            cfg,
            Some(TurnRolloutEntry {
                a: TurnRolloutPilotEntry {
                    goal_credit: -17,
                    goal_count: 3,
                    end_turn: 9,
                    death_save_use_count: 2,
                    ..e_ds
                },
                b: e_ev,
            }),
        );
        let cached = cache.get(h, cfg).flatten().unwrap();
        assert_eq!(cached.a.goal_credit, -17);
        assert_eq!(cached.a.goal_count, 3);
        assert_eq!(cached.a.end_turn, 9);
        assert_eq!(cached.a.death_save_use_count, 2);
        assert_eq!(cached.b.outcome_rank(), 1);
    }

    #[test]
    fn pick_rollout_choice_hand_vs_library_and_guards_parity() {
        use sts2_content::Sts2Content;
        use sts2_kernel::continuation::{ChoiceRequest, ChoiceTiming};
        use sts2_types::{CharacterId, MonsterId};

        let content = Sts2Content;
        let mut state = build_test_combat(
            CharacterId::Ironclad,
            &[CardId::StrikeIronclad],
            &[],
            &[(MonsterId::FuzzyWurmCrawler, 30)],
        );
        state.player.energy = 3;

        // 1. Armaments (beneficial HandCard upgrade prompt): un-upgraded Inflame outranks already-upgraded DemonForm+ (-300 penalty)!
        let (hand, draw) = set_hand_and_draw(
            &mut state,
            &content,
            &[
                (CardId::DemonForm, 1),    // idx 0: already upgraded Rare Power (gets -300 penalty on upgrade prompt)
                (CardId::Inflame, 0),      // idx 1: un-upgraded Uncommon Power
                (CardId::DefendIronclad, 0), // idx 2: un-upgraded Defend (60 + 15 = 75)
                (CardId::AscendersBane, 0),  // idx 3: Curse (-400)
            ],
            &[
                (CardId::DemonForm, 0),    // draw idx 0: high value Rare Power
                (CardId::StrikeIronclad, 0), // draw idx 1: basic Strike
            ],
        );
        let mut scored = Vec::new();
        let mut sel = SmallVec::new();
        let req_arm = ChoiceRequest {
            timing: ChoiceTiming::Action,
            source_id: "CARD.ARMAMENTS".into(),
            source_pile: PileType::Hand,
            min_count: 1,
            max_count: 1,
            candidates: SmallVec::from_slice(&hand),
            can_skip: false,
        };
        assert!(pick_rollout_choice(
            &mut state,
            &content,
            &[],
            &req_arm,
            0,
            false,
            &mut scored,
            &mut sel
        ));
        assert_eq!(sel.as_slice(), &[1]);

        // 2. Nightmare (beneficial HandCard copy prompt): picks highest quality (upgraded DemonForm).
        let req_night = ChoiceRequest {
            timing: ChoiceTiming::Action,
            source_id: "CARD.NIGHTMARE".into(),
            source_pile: PileType::Hand,
            min_count: 1,
            max_count: 1,
            candidates: SmallVec::from_slice(&hand),
            can_skip: false,
        };
        assert!(pick_rollout_choice(
            &mut state,
            &content,
            &[],
            &req_night,
            0,
            false,
            &mut scored,
            &mut sel
        ));
        assert_eq!(sel.as_slice(), &[0]);

        // 3. Purity (destructive optional HandCard multi-pick up to cap): selects Curse + Defend (<=75) and skips DemonForm+/Inflame!
        let req_purity = ChoiceRequest {
            timing: ChoiceTiming::Action,
            source_id: "CARD.PURITY".into(),
            source_pile: PileType::Hand,
            min_count: 0,
            max_count: 3,
            candidates: SmallVec::from_slice(&hand),
            can_skip: true,
        };
        assert!(pick_rollout_choice(
            &mut state,
            &content,
            &[],
            &req_purity,
            0,
            false,
            &mut scored,
            &mut sel
        ));
        assert_eq!(sel.as_slice(), &[3, 2]);

        // 4. Charge / Cleanse / Seance (destructive DrawPile): picks Strike over DemonForm!
        let req_cleanse = ChoiceRequest {
            timing: ChoiceTiming::Action,
            source_id: "CARD.CLEANSE".into(),
            source_pile: PileType::Draw,
            min_count: 1,
            max_count: 1,
            candidates: SmallVec::from_slice(&draw),
            can_skip: false,
        };
        assert!(pick_rollout_choice(
            &mut state,
            &content,
            &[],
            &req_cleanse,
            0,
            false,
            &mut scored,
            &mut sel
        ));
        assert_eq!(sel.as_slice(), &[1]);

        // 5. Guards (Regent destructive optional HandCard):
        // Under heavy threat, Defend's urgency boost is suppressed for Guards (eval_unblocked_incoming=0)
        // so Defend remains <= 80 and IS eligible, while SovereignBlade is skipped!
        // With unblocked_incoming = 0, Guards skips all positive-quality cards (max_allowed = -1).
        let (guards_hand, _) = set_hand_and_draw(
            &mut state,
            &content,
            &[
                (CardId::SovereignBlade, 0),
                (CardId::DefendIronclad, 0),
                (CardId::StrikeIronclad, 0),
            ],
            &[],
        );
        let req_guards = ChoiceRequest {
            timing: ChoiceTiming::Action,
            source_id: "CARD.GUARDS".into(),
            source_pile: PileType::Hand,
            min_count: 0,
            max_count: 5,
            candidates: SmallVec::from_slice(&guards_hand),
            can_skip: true,
        };
        assert!(pick_rollout_choice(
            &mut state,
            &content,
            &[],
            &req_guards,
            30,
            false,
            &mut scored,
            &mut sel
        ));
        assert_eq!(sel.as_slice(), &[1, 2]);
        assert!(pick_rollout_choice(
            &mut state,
            &content,
            &[],
            &req_guards,
            0,
            false,
            &mut scored,
            &mut sel
        ));
        assert!(sel.is_empty());
    }

    #[test]
    fn pilot_weak_three_tier_block_powers_hold_guards_and_brightest_flame() {
        use sts2_content::Sts2Content;
        use sts2_kernel::PowerInst;
        use sts2_types::{CharacterId, MonsterId};

        let content = Sts2Content;
        let mut playable_buf = Vec::new();
        let mut hand_buf = Vec::new();
        let mut seen_keys = Vec::new();
        let mut intent_cache = IntentHitsCache::new();

        // 1. Weak + 3-tier block + Power scaling under lethal vs zero threat.
        let mut state = build_test_combat(
            CharacterId::Silent,
            &[CardId::Neutralize],
            &[],
            &[(MonsterId::FuzzyWurmCrawler, 50)],
        );
        state.player.energy = 3;
        // FuzzyWurmCrawler deals 4 on turn 1; add +20 Strength -> 24 incoming damage.
        state.creatures[1].powers.push(PowerInst {
            handle: 100,
            id: PowerId::StrengthPower,
            amount: 20,
            amount_on_turn_start: 20,
            skip_next_duration_tick: false,
            applier_handle: None,
            ext_index: None,
        });
        let root_baseline = RootEvalBaseline::default();
        // Set player HP = 20 so 24 incoming is LETHAL (block_mult = 260, weak_mult = 220).
        state.creatures[0].current_hp = 20;
        state.player.current_hp = 20;
        set_hand_and_draw(
            &mut state,
            &content,
            &[
                (CardId::StrikeSilent, 0),  // 6 dmg -> 6*45 = 270
                (CardId::DefendSilent, 0),  // 5 blk -> 5*260+450 = 1750
                (CardId::Neutralize, 0),    // 3 dmg + Weak -> 135 + 420 + (24/4)*220 = 1875
            ],
            &[],
        );
        let (mv_weak, _) = collect_and_select_pilot_move_slow(
            &mut state,
            &content,
            &root_baseline,
            None,
            &mut intent_cache,
            PilotKind::DefensiveTempo,
            &[],
            &mut playable_buf,
            &mut hand_buf,
            &mut seen_keys,
        );
        // With APPLIES_WEAK wired, Neutralize wins on lethal threat!
        assert_eq!(state.card(mv_weak.unwrap().0).unwrap().id, CardId::Neutralize);

        // Compare Barricade excess block preservation (*28) vs no Barricade (*6) when unblocked == 0.
        state.player.block = 40;
        state.creatures[0].block = 40;
        let ctx_no_bar =
            compute_fast_turn_context::<false>(&state, &content, &root_baseline, &mut intent_cache);
        assert!(!ctx_no_bar.preserves_excess_block);
        assert_eq!(ctx_no_bar.unblocked_incoming, 0);

        state.creatures[0].powers.push(PowerInst {
            handle: 101,
            id: PowerId::BarricadePower,
            amount: 1,
            amount_on_turn_start: 0,
            skip_next_duration_tick: false,
            applier_handle: None,
            ext_index: None,
        });
        state.hook_cache = Default::default();
        let _ = state.ensure_hook_cache(&content);
        let ctx_bar =
            compute_fast_turn_context::<false>(&state, &content, &root_baseline, &mut intent_cache);
        assert!(ctx_bar.preserves_excess_block);

        // 2. Hold guards: CalculatedGamble in 1-card hand, GrandFinale with non-empty draw,
        // Nightmare with empty/better-in-draw target, Retained pure-block with unblocked == 0.
        let mut hold_state = build_test_combat(
            CharacterId::Silent,
            &[CardId::CalculatedGamble],
            &[],
            &[(MonsterId::FuzzyWurmCrawler, 40)],
        );
        hold_state.player.energy = 3;
        set_hand_and_draw(
            &mut hold_state,
            &content,
            &[(CardId::CalculatedGamble, 0)],
            &[(CardId::StrikeSilent, 0)],
        );
        let (mv_cg, _) = collect_and_select_pilot_move_slow(
            &mut hold_state,
            &content,
            &root_baseline,
            None,
            &mut intent_cache,
            PilotKind::DefensiveTempo,
            &[],
            &mut playable_buf,
            &mut hand_buf,
            &mut seen_keys,
        );
        assert!(mv_cg.is_none(), "CalculatedGamble in 1-card hand must be held");

        // Retained Defend with RetainHandPower and unblocked == 0 is held, and 1-card shortcut refuses!
        hold_state.player.block = 20;
        hold_state.creatures[0].block = 20;
        hold_state.creatures[0].powers.push(PowerInst {
            handle: 102,
            id: PowerId::RetainHandPower,
            amount: 1,
            amount_on_turn_start: 0,
            skip_next_duration_tick: false,
            applier_handle: None,
            ext_index: None,
        });
        set_hand_and_draw(&mut hold_state, &content, &[(CardId::DefendSilent, 0)], &[]);
        let def_prof = hold_state.fast_card_rollout_profile(&content, CardId::DefendSilent);
        assert!(!can_use_single_card_shortcut(
            &hold_state,
            &root_baseline,
            CardId::DefendSilent,
            false,
            &def_prof,
            None,
        ));
        let (mv_ret_blk, _) = collect_and_select_pilot_move_slow(
            &mut hold_state,
            &content,
            &root_baseline,
            None,
            &mut intent_cache,
            PilotKind::DefensiveTempo,
            &[],
            &mut playable_buf,
            &mut hand_buf,
            &mut seen_keys,
        );
        assert!(mv_ret_blk.is_none(), "Retained pure block must be held when covered");

        // Nightmare skips when Draw contains a much higher-setup card (DemonForm) while Hand only has Strike!
        set_hand_and_draw(
            &mut hold_state,
            &content,
            &[(CardId::Nightmare, 0), (CardId::StrikeSilent, 0)],
            &[(CardId::DemonForm, 0)],
        );
        assert!(should_hold_or_skip_copy_card(
            &hold_state,
            &content,
            hold_state.player.hand[0],
            false,
            true,
            false,
        ));

        // 3. BrightestFlame cap enforcement.
        hold_state.semantic_history.brightest_flame_max_hp_spent = 2;
        let bf_baseline = RootEvalBaseline {
            initial_brightest_flame_max_hp_spent: 2,
            root_state_brightest_flame_spent: 2,
            ..Default::default()
        };
        assert!(is_brightest_flame_blocked(
            &hold_state,
            &bf_baseline,
            CardId::BrightestFlame,
            Some(2),
        ));
        assert!(!is_brightest_flame_blocked(
            &hold_state,
            &bf_baseline,
            CardId::BrightestFlame,
            Some(4),
        ));

        // 4. Verify 3-tier block multipliers (260 / 185 / 135) & Barricade *28 vs *6 excess bonus & Inflame scaling.
        let mut tier_state = build_test_combat(
            CharacterId::Ironclad,
            &[CardId::DefendIronclad],
            &[],
            &[(MonsterId::FuzzyWurmCrawler, 60)],
        );
        tier_state.player.energy = 3;
        // FuzzyWurmCrawler deals 4 dmg + StrengthPower => tune unblocked incoming cleanly.
        tier_state.creatures[0].max_hp = 80;
        tier_state.player.max_hp = 80;
        set_hand_and_draw(
            &mut tier_state,
            &content,
            &[(CardId::DefendIronclad, 0), (CardId::Inflame, 0)],
            &[],
        );
        let ctx_zero = compute_fast_turn_context::<false>(
            &tier_state,
            &content,
            &root_baseline,
            &mut intent_cache,
        );
        assert_eq!(MoveScorePrecomp::new::<false>(&ctx_zero, false).block_mult, 135);
        // With 24 incoming and 18 player HP => lethal_threat (block_mult = 260), Defend (5*260+450 = 1750)
        // outscores Power under lethal threat (Inflame only gets 80 + 280 under lethal_threat)!
        tier_state.creatures[1].powers.push(PowerInst {
            handle: 200,
            id: PowerId::StrengthPower,
            amount: 20,
            amount_on_turn_start: 20,
            skip_next_duration_tick: false,
            applier_handle: None,
            ext_index: None,
        });
        tier_state.hook_cache = Default::default();
        let _ = tier_state.ensure_hook_cache(&content);
        tier_state.creatures[0].current_hp = 18;
        tier_state.player.current_hp = 18;
        let (mv_lethal, _) = collect_and_select_pilot_move_slow(
            &mut tier_state,
            &content,
            &root_baseline,
            None,
            &mut intent_cache,
            PilotKind::DefensiveTempo,
            &[],
            &mut playable_buf,
            &mut hand_buf,
            &mut seen_keys,
        );
        assert_eq!(
            tier_state.card(mv_lethal.unwrap().0).unwrap().id,
            CardId::DefendIronclad
        );
        let ctx_lethal = compute_fast_turn_context::<false>(
            &tier_state,
            &content,
            &root_baseline,
            &mut intent_cache,
        );
        assert_eq!(MoveScorePrecomp::new::<false>(&ctx_lethal, false).block_mult, 260);
        // Critical danger (30 HP, 24 unblocked => 6*4 = 24 < 80): block_mult = 185.
        tier_state.creatures[0].current_hp = 30;
        tier_state.player.current_hp = 30;
        let ctx_crit = compute_fast_turn_context::<false>(
            &tier_state,
            &content,
            &root_baseline,
            &mut intent_cache,
        );
        assert_eq!(MoveScorePrecomp::new::<false>(&ctx_crit, false).block_mult, 185);
        // Moderate danger (80 HP, 24 unblocked => 56*4 > 80): block_mult = 135.
        tier_state.creatures[0].current_hp = 80;
        tier_state.player.current_hp = 80;
        let ctx_mod = compute_fast_turn_context::<false>(
            &tier_state,
            &content,
            &root_baseline,
            &mut intent_cache,
        );
        assert_eq!(MoveScorePrecomp::new::<false>(&ctx_mod, false).block_mult, 135);
    }

    #[test]
    fn choice_probe_materialization_and_rank_encoding() {
        use sts2_content::Sts2Content;
        use sts2_types::{CharacterId, MonsterId};

        let win_high_goal = ChoiceProbeResult {
            score: 1000.0,
            hp_lost: 2,
            setup_score: 40,
            board_investment: 12,
            strategy_goal_hp_credit: 5,
            strategic_hp_credit: 5,
            is_victory: true,
            is_defeat: false,
            is_event_defeat: false,
        };
        let win_no_goal = ChoiceProbeResult {
            strategy_goal_hp_credit: 0,
            strategic_hp_credit: 0,
            ..win_high_goal
        };
        let ongoing = ChoiceProbeResult {
            score: 1000.0,
            is_victory: false,
            is_defeat: false,
            is_event_defeat: false,
            ..win_no_goal
        };
        let ev_defeat = ChoiceProbeResult {
            score: -500.0,
            is_event_defeat: true,
            ..ongoing
        };
        let defeat = ChoiceProbeResult {
            score: 5000.0,
            is_defeat: true,
            is_event_defeat: false,
            ..ongoing
        };

        let r_goal = encode_choice_probe_rank_score(&win_high_goal);
        let r_win = encode_choice_probe_rank_score(&win_no_goal);
        let r_ong = encode_choice_probe_rank_score(&ongoing);
        let r_ev = encode_choice_probe_rank_score(&ev_defeat);
        let r_def = encode_choice_probe_rank_score(&defeat);

        assert!((r_goal - r_win - 500_000.0).abs() < 1e-6);
        assert!(r_win > r_ong && r_ong > r_ev && r_ev > r_def);
        assert_eq!(r_ev, -1_500_000_000_000_000.0 - 500.0);
        assert_eq!(r_def, -2_000_000_000_000_000.0 + 5000.0);

        // End-to-end probe materialization: setup_score, strategy_goal_hp_credit gating, and event defeat flag.
        let content = Sts2Content;
        let mut base = build_test_combat(
            CharacterId::Ironclad,
            &[CardId::Inflame],
            &[],
            &[(MonsterId::FuzzyWurmCrawler, 120)],
        );
        base.player.energy = 3;
        set_hand_and_draw(&mut base, &content, &[(CardId::Inflame, 0)], &[]);
        let snap = base.clone();
        let mut ws = ChoiceProbeWorkspace::new(base.static_ctx.clone());
        let mut cache = ChoiceProbeCache::new();
        let mut access = ProbeCacheAccess::SingleThread(&mut cache);
        let rb_no_strat =
            RootEvalBaseline::from_state_with_policy(&base, &content, &SearchPolicy::default());
        let profile = SolverSearchProfile::default();
        let probe_setup = evaluate_choice_probe_in_place_with_restore(
            &mut ws,
            &mut base,
            |s| {
                clone_battle_state_into(&snap, s);
                true
            },
            hash_canonical_key(&snap),
            &mut access,
            &content,
            &rb_no_strat,
            &profile,
            EvalMode::Balanced,
            0,
            0,
            0,
            0,
        )
        .expect("choice probe succeeds");
        assert!(!probe_setup.is_victory && !probe_setup.is_defeat && !probe_setup.is_event_defeat);
        assert!(probe_setup.setup_score > 0);
        assert_eq!(probe_setup.strategy_goal_hp_credit, 0);

        let mut ev_state = build_test_combat(
            CharacterId::Ironclad,
            &[CardId::StrikeIronclad],
            &[],
            &[(MonsterId::FuzzyWurmCrawler, 50)],
        );
        ev_state.battleworn_dummy_timed_out = true;
        ev_state.is_in_progress = false;
        let ev_snap = ev_state.clone();
        let mut ev_cache = ChoiceProbeCache::new();
        let mut ev_access = ProbeCacheAccess::SingleThread(&mut ev_cache);
        let probe_ev = evaluate_choice_probe_in_place_with_restore(
            &mut ws,
            &mut ev_state,
            |s| {
                clone_battle_state_into(&ev_snap, s);
                true
            },
            hash_canonical_key(&ev_snap),
            &mut ev_access,
            &content,
            &rb_no_strat,
            &profile,
            EvalMode::Balanced,
            0,
            0,
            0,
            0,
        )
        .expect("event defeat probe succeeds");
        assert!(!probe_ev.is_victory && !probe_ev.is_defeat && probe_ev.is_event_defeat);
    }
}
