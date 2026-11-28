use crate::arena::{NodeArena, NodeId};
use crate::eval::{
    weights, ActEndingBossPolicy, BossHpRelief, PostCombatRelicHealProfile, RelicCounterId,
    RootEvalBaseline, SolverPotionPolicy, SolverTheftPolicy, FAST_FLAG_FORFEITS_GOLD,
};
use crate::portfolio::BeamWidthPortfolio;
use hashbrown::HashMap;
use parking_lot::RwLock;
use std::sync::atomic::{AtomicI32, Ordering};
use sts2_kernel::state::BattleState;
use sts2_kernel::ContentDispatcher;
use sts2_types::{
    CardId, CardStaticFlags, PileType, PotionId, PowerStaticFlags, RelicId, RelicStaticFlags,
};

const PER_NICHE_PARETO_TOP_K: usize = 32;
const NICHE_SLOT_COUNT: usize = 7; // 0=Primary, 1=Survival, 2=Tempo, 3=BoardInvestment, 4=Optionality, 5=PureRollout, 6=Strategy
const MAX_DISTINCT_POTION_SIGNATURES: usize = 16;

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ExtendedPruneEntry {
    pub effective_net_hp_loss_centi: i32,
    pub tactical_sig: u64,
    pub node_id: NodeId,
    pub primary: f64,
    pub aux_score: f64,
    pub forced_used_mask: u64,
    pub relic_satisfied_mask: u64,
    pub best_hp: i32,
    pub effective_defense: i32,
    pub enemy_hp: i32,
    pub rem_enemies: i32,
    pub board_investment: i32,
    pub setup_score: i32,
    pub max_dark_evoke: i32,
    pub total_poison_and_doom: i32,
    pub optionality_score: i32,
    pub strategy_goal_hp_credit: i32,
    pub relic_distance: i32,
    pub relic_hp_credit: i32,
    pub relic_healing_hp_credit: i32,
    pub relic_satisfied_priority: i32,
    pub growth_hp_credit: i32,
    pub growth_reward_count: i32,
    pub used_potion_slot_mask: u16,
    pub potion_cost: i16,
    pub strategic_hp_credit: i16,
    pub energy: i16,
    pub outstanding_stolen: i16,
    pub playable_hand_count: u8,
    pub optional_paid_potion_count: u8,
    pub explicit_potion_count: u8,
    pub death_save_use_count: u8,
    pub strategy_goal_count: u8,
    pub forfeits_gold: bool,
    pub has_rollout_score: bool,
    pub has_choice_lookahead: bool,
}

#[inline(always)]
const fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

#[allow(clippy::too_many_arguments)]
#[inline]
pub(crate) fn compute_effective_net_hp_loss_centi(
    root: &RootEvalBaseline,
    has_meat_on_the_bone_target: bool,
    player_hp: i32,
    player_max_hp: i32,
    projected_player_hp: i32,
    cumulative_hp_lost: i32,
    recovered_hp: i32,
    death_save_hp_restored: i32,
    death_save_use_count: u8,
    feed_count: u8,
    won: bool,
) -> i32 {
    let hp_scale = ActEndingBossPolicy::persistent_hp_scale(root.boss_hp_relief);
    let death_save_hp = death_save_hp_restored.max(0);
    let max_hp_delta = if root.initial_player_max_hp > 0 {
        root.initial_player_max_hp - player_max_hp
    } else {
        0
    };
    let raw_hp_loss = if root.initial_player_max_hp > 0 {
        let end_max_hp = (root.initial_player_max_hp - max_hp_delta).max(1);
        let mut projected_heal = 0i32;
        if player_hp > 0
            && (root.post_combat_relic_heal.unconditional_heal > 0
                || root.post_combat_relic_heal.wounded_heal > 0)
        {
            if won {
                projected_heal = root
                    .post_combat_relic_heal
                    .monotone_heal_for(player_hp, end_max_hp);
            } else if has_meat_on_the_bone_target
                && death_save_use_count == 0
                && projected_player_hp >= player_hp
            {
                let safe_floor = (end_max_hp / 4).min(12).max(4);
                if player_hp >= safe_floor {
                    let wounded_bonus = root.post_combat_relic_heal.heal_for(player_hp, end_max_hp)
                        - root
                            .post_combat_relic_heal
                            .monotone_heal_for(player_hp, end_max_hp);
                    if wounded_bonus > 0 {
                        projected_heal = wounded_bonus;
                    }
                }
            }
            let max_cap = (end_max_hp - player_hp).max(0);
            if projected_heal > max_cap {
                projected_heal = max_cap;
            }
        }
        (root.initial_player_hp - player_hp) + death_save_hp - projected_heal
    } else {
        cumulative_hp_lost - recovered_hp + death_save_hp
    };

    let max_hp_term_centi = if max_hp_delta >= 0 {
        if root.boss_hp_relief != BossHpRelief::RunEnding {
            max_hp_delta * weights::MAX_HP_LOSS_WEIGHT_CENTI
        } else {
            0
        }
    } else {
        let max_hp_gain = -max_hp_delta;
        let feed_excluded = i32::from(feed_count) * root.feed_max_hp_per_kill;
        let credited_gain = (max_hp_gain - feed_excluded).max(0);
        -(credited_gain * weights::MAX_HP_GAIN_WEIGHT_CENTI)
    };

    raw_hp_loss * hp_scale + max_hp_term_centi
}

#[allow(clippy::too_many_arguments)]
#[inline]
pub(crate) fn compute_tactical_signature(
    turn: i16,
    used_potion_slot_mask: u16,
    potion_count: u8,
    death_save_use_count: u8,
    alive_enemy_count: u8,
    reviving_enemy_count: u8,
    energy: i16,
    cumulative_hp_lost: i32,
    recovered_hp: i32,
    best_hp: i32,
    enemy_hp: i32,
    player_max_hp_delta: i16,
    effective_defense: i32,
    optionality_score: i32,
    playable_hand_count: u8,
    setup_score: i32,
    board_investment: i32,
    max_dark_evoke: i32,
    total_poison_and_doom: i32,
    has_pure_growth_targets: bool,
    growth_hp_credit: i32,
    growth_reward_count: i32,
    alchemize_count: u8,
    feed_count: u8,
    has_relic_targets: bool,
    relic_satisfied_mask: u64,
    relic_distance: i32,
    relic_satisfied_priority: i32,
    relic_hp_credit: i32,
    preserve_theft: bool,
    outstanding_stolen: i16,
    forfeits_gold: bool,
) -> u64 {
    let energy_u8 = energy.clamp(0, u8::MAX as i16) as u8;
    let w0 = ((turn.clamp(0, u8::MAX as i16) as u8) as u64)
        | ((used_potion_slot_mask as u64) << 8)
        | ((potion_count as u64) << 24)
        | ((death_save_use_count as u64) << 32)
        | ((alive_enemy_count as u64) << 40)
        | ((reviving_enemy_count as u64) << 48)
        | ((energy_u8 as u64) << 56);
    let w1 = ((cumulative_hp_lost as u32) as u64) | (((recovered_hp as u32) as u64) << 32);
    let w2 = ((best_hp as u32) as u64) | (((enemy_hp as u32) as u64) << 32);
    let w3 = ((player_max_hp_delta as u16) as u64)
        | ((((effective_defense.clamp(i16::MIN as i32, i16::MAX as i32) as i16) as u16) as u64)
            << 16)
        | ((((optionality_score.clamp(i16::MIN as i32, i16::MAX as i32) as i16) as u16) as u64)
            << 32)
        | ((playable_hand_count as u64) << 48);
    let w4 = ((setup_score as u32) as u64) | (((board_investment as u32) as u64) << 32);
    let w5 = ((max_dark_evoke as u32) as u64) | (((total_poison_and_doom as u32) as u64) << 32);

    let mut h = 0xD1B5_4A32_D192_ED03u64;
    h = splitmix64(h ^ w0);
    h = splitmix64(h ^ w1);
    h = splitmix64(h ^ w2);
    h = splitmix64(h ^ w3);
    h = splitmix64(h ^ w4);
    h = splitmix64(h ^ w5);
    if preserve_theft {
        let w_theft = ((outstanding_stolen as u16) as u64)
            | if forfeits_gold { 1u64 << 16 } else { 0u64 };
        h = splitmix64(h ^ w_theft);
    }
    if has_pure_growth_targets {
        let w_growth = (((growth_hp_credit.clamp(i16::MIN as i32, i16::MAX as i32) as i16) as u16)
            as u64)
            | (((growth_reward_count.clamp(0, u16::MAX as i32) as u16) as u64) << 16)
            | ((alchemize_count as u64) << 32)
            | ((feed_count as u64) << 40);
        h = splitmix64(h ^ w_growth);
    }
    if has_relic_targets {
        let w_relic = ((relic_satisfied_mask as u16) as u64)
            | (((relic_distance.clamp(0, u16::MAX as i32) as u16) as u64) << 16)
            | ((((relic_satisfied_priority.clamp(i16::MIN as i32, i16::MAX as i32) as i16) as u16)
                as u64)
                << 32)
            | ((((relic_hp_credit.clamp(i16::MIN as i32, i16::MAX as i32) as i16) as u16) as u64)
                << 48);
        h = splitmix64(h ^ w_relic);
    }
    if h == 0 {
        1
    } else {
        h
    }
}

#[inline(always)]
fn compute_potion_bucket(
    potion_count: u8,
    explicit_potion_count: u8,
    optional_paid_potion_count: u8,
    forced_used_mask: u64,
    root: &RootEvalBaseline,
    force_explicit_nonzero_bucket: bool,
) -> usize {
    let mut count = if root.initial_player_max_hp <= 0 {
        if optional_paid_potion_count > 0 {
            optional_paid_potion_count as usize
        } else {
            potion_count as usize
        }
    } else {
        let mut c = optional_paid_potion_count as usize;
        if root.forced_directive_count > 0 || root.all_forced_potion_mask != 0 {
            c += (forced_used_mask & root.all_forced_potion_mask).count_ones() as usize;
        }
        if c == 0
            && root.effective_potion_policy == SolverPotionPolicy::RequireAtLeastOne
            && explicit_potion_count > 0
        {
            c = 1;
        }
        c
    };
    if force_explicit_nonzero_bucket {
        if explicit_potion_count == 0 {
            count = 0;
        } else if count == 0 {
            count = 1;
        }
    }
    count.min(3)
}

const PRUNE_FLAG_HAS_ROLLOUT: u8 = 1 << 0;
const PRUNE_FLAG_HAS_CHOICE_LA: u8 = 1 << 1;
const PRUNE_FLAG_FAST_EXACT: u8 = 1 << 2;

#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub(crate) struct PruneKey {
    pub primary: f64,
    pub aux_score: f64,
    pub forced_used_mask: u64,
    pub node_id: NodeId,
    pub enemy_hp: i32,
    pub effective_hp_loss: i32,
    pub total_defense_hp: i32,
    pub optionality_score: i32,
    pub used_potion_slot_mask: u16,
    pub strategic_hp_credit: i16,
    pub potion_cost: i16,
    pub cumulative_hp_lost: i16,
    pub best_hp: i16,
    pub board_investment: i16,
    pub setup_score: i16,
    pub rem_enemies: u8,
    pub explicit_potion_count: u8,
    pub forced_match_count: u8,
    pub death_save_use_count: u8,
    pub strategy_goal_count: u8,
    pub flags: u8,
}

const _: () = {
    assert!(std::mem::size_of::<PruneKey>() == 64);
};

impl PruneKey {
    #[inline(always)]
    pub(crate) const fn has_rollout_score(&self) -> bool {
        (self.flags & PRUNE_FLAG_HAS_ROLLOUT) != 0
    }

    #[inline(always)]
    pub(crate) const fn has_choice_lookahead(&self) -> bool {
        (self.flags & PRUNE_FLAG_HAS_CHOICE_LA) != 0
    }

    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_raw_fields(
        nid: NodeId,
        root: &RootEvalBaseline,
        use_rollout_scores: bool,
        enemy_hp: i32,
        player_hp: i16,
        player_max_hp: i16,
        player_block: i16,
        projected_player_hp: i16,
        cumulative_hp_lost: i16,
        recovered_hp: i16,
        strategic_hp_credit: i16,
        alive_enemy_count: u8,
        reviving_enemy_count: u8,
        death_save_use_count: u8,
        strategy_goal_count: u8,
        feed_count: u8,
        energy: i16,
        stars: i16,
        forced_used_mask: u64,
        used_potion_slot_mask: u16,
        potion_cost: i16,
        explicit_potion_count: u8,
        has_full_eval: bool,
        beam_rank_score: f64,
        has_rollout_score: bool,
        has_choice_lookahead: bool,
        stored_aux_score: f64,
        effective_defense: i16,
        board_investment_raw: i16,
        setup_score_raw: i16,
        optionality_cards: i16,
    ) -> (Self, i64, i64, i64, i64, f64) {
        let max_hp_delta = root.initial_player_max_hp - (player_max_hp as i32);
        let effective_hp_loss = (cumulative_hp_lost as i32)
            - (recovered_hp as i32)
            + if max_hp_delta < 0 {
                let non_feed = ((-max_hp_delta) - i32::from(feed_count) * root.feed_max_hp_per_kill).max(0);
                -(non_feed * 2)
            } else {
                max_hp_delta
            };
        let best_hp = player_hp.max(projected_player_hp);
        let best_hp_i32 = best_hp as i32;
        let (
            primary,
            aux_score,
            total_defense_hp,
            board_investment,
            setup_score,
            optionality_score,
        ) = if has_full_eval {
            let primary = if use_rollout_scores && has_rollout_score {
                0.35 * beam_rank_score + 0.65 * stored_aux_score
            } else {
                beam_rank_score
            };
            let aux = if has_rollout_score || has_choice_lookahead {
                stored_aux_score
            } else {
                f64::NEG_INFINITY
            };
            (
                primary,
                aux,
                best_hp_i32 + (effective_defense as i32),
                board_investment_raw,
                setup_score_raw,
                (energy as i32).max(0) + (stars as i32).max(0) + (optionality_cards as i32),
            )
        } else {
            (
                beam_rank_score,
                f64::NEG_INFINITY,
                best_hp_i32 + (player_block as i32),
                0,
                0,
                (energy as i32).max(0) + (stars as i32).max(0),
            )
        };
        let death_save_use_count_i32 = death_save_use_count as i32;
        let rem_enemies_i32 = (alive_enemy_count as i32) + (reviving_enemy_count as i32);
        let board_i32 = board_investment as i32;
        let setup_i32 = setup_score as i32;
        let fast_keys_exact = (0..=63).contains(&death_save_use_count_i32)
            && (-4096..=12287).contains(&effective_hp_loss)
            && (-2048..=14335).contains(&total_defense_hp)
            && (-2048..=14335).contains(&best_hp_i32)
            && (0..=16383).contains(&enemy_hp)
            && (0..=63).contains(&rem_enemies_i32)
            && (-524_288..=524_287).contains(&board_i32)
            && (-524_288..=524_287).contains(&setup_i32)
            && (-524_288..=524_287).contains(&optionality_score);
        let (survival_fast, tempo_fast, setup_fast, optionality_fast) = if fast_keys_exact {
            let ds_bits = ((63 - death_save_use_count_i32) as i64) << 56;
            let ehl_surv = ((12287 - effective_hp_loss) as i64) << 42;
            let tdh_bits = ((total_defense_hp + 2048) as i64) << 28;
            let bhp_bits = ((best_hp_i32 + 2048) as i64) << 14;
            let ehp_surv = (16383 - enemy_hp) as i64;
            let s = ds_bits | ehl_surv | tdh_bits | bhp_bits | ehp_surv;

            let rem_bits = ((63 - rem_enemies_i32) as i64) << 42;
            let ehp_tempo = ((1_048_575 - enemy_hp) as i64) << 22;
            let ehl_20 = (524_287 - effective_hp_loss) as i64;
            let t = rem_bits | ehp_tempo | ehl_20;

            let bi_bits = ((board_i32 + 524_288) as i64) << 40;
            let ehl_mid = ehl_20 << 20;
            let ss_bits = (setup_i32 + 524_288) as i64;
            let su = bi_bits | ehl_mid | ss_bits;

            let os_bits = ((optionality_score + 524_288) as i64) << 40;
            let bi_mid = ((board_i32 + 524_288) as i64) << 20;
            let o = os_bits | bi_mid | ehl_20;
            (s, t, su, o)
        } else {
            (0, 0, 0, 0)
        };
        let mut flags = 0u8;
        if has_full_eval && has_rollout_score {
            flags |= PRUNE_FLAG_HAS_ROLLOUT;
        }
        if has_full_eval && has_choice_lookahead {
            flags |= PRUNE_FLAG_HAS_CHOICE_LA;
        }
        if fast_keys_exact {
            flags |= PRUNE_FLAG_FAST_EXACT;
        }
        let aux_f64 = if use_rollout_scores {
            if (flags & PRUNE_FLAG_HAS_ROLLOUT) != 0 {
                aux_score
            } else {
                f64::NEG_INFINITY
            }
        } else if (flags & PRUNE_FLAG_HAS_CHOICE_LA) != 0 {
            aux_score
        } else {
            f64::NEG_INFINITY
        };
        let key = Self {
            primary,
            aux_score,
            forced_used_mask,
            node_id: nid,
            enemy_hp,
            effective_hp_loss,
            total_defense_hp,
            optionality_score,
            used_potion_slot_mask,
            strategic_hp_credit,
            potion_cost,
            cumulative_hp_lost,
            best_hp,
            board_investment,
            setup_score,
            rem_enemies: rem_enemies_i32.clamp(0, u8::MAX as i32) as u8,
            explicit_potion_count,
            forced_match_count: (forced_used_mask & root.all_forced_potion_mask).count_ones()
                as u8,
            death_save_use_count,
            strategy_goal_count,
            flags,
        };
        (
            key,
            survival_fast,
            tempo_fast,
            setup_fast,
            optionality_fast,
            aux_f64,
        )
    }

    #[inline]
    fn from_node(
        arena: &NodeArena,
        nid: NodeId,
        root: &RootEvalBaseline,
        use_rollout_scores: bool,
    ) -> (Self, i64, i64, i64, i64, f64, crate::hash::StateHash) {
        let n = arena.get(nid);
        let f = &n.fast_eval;
        let (k, s, t, su, o, a) = Self::from_raw_fields(
            nid,
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
            n.forced_used_mask,
            n.used_potion_slot_mask,
            n.potion_strategic_cost,
            n.explicit_potion_count,
            n.has_full_eval(),
            n.beam_rank_score,
            n.has_rollout_score(),
            n.has_choice_lookahead(),
            n.rollout_score,
            n.effective_defense,
            n.board_investment,
            n.setup_score,
            n.optionality_cards,
        );
        (k, s, t, su, o, a, n.state_hash)
    }
}

pub(crate) trait PruneSource: Sync {
    fn build_prune_entry(
        &self,
        id: NodeId,
        root: &RootEvalBaseline,
        use_rollout_scores: bool,
    ) -> (PruneKey, i64, i64, i64, i64, f64, crate::hash::StateHash);
    fn potion_count(&self, id: NodeId) -> u8;
    fn explicit_potion_count(&self, id: NodeId) -> u8;
    fn beam_score(&self, id: NodeId, use_rollout_scores: bool) -> f64;
    fn tie_break_fields(&self, id: NodeId) -> (i16, i32, bool, f64, crate::hash::StateHash);

    #[inline(always)]
    fn optional_paid_potion_count(&self, id: NodeId) -> u8 {
        self.potion_count(id)
    }

    #[inline(always)]
    fn forced_used_mask(&self, _id: NodeId) -> u64 {
        0
    }

    #[inline(always)]
    fn node_turn(&self, _id: NodeId) -> i32 {
        0
    }

    #[inline]
    fn build_extended_prune_entry(
        &self,
        id: NodeId,
        k: &PruneKey,
        root: &RootEvalBaseline,
        _use_rollout_scores: bool,
    ) -> ExtendedPruneEntry {
        let eff_def = k.total_defense_hp - (k.best_hp as i32);
        let eff_loss = k.effective_hp_loss * weights::HP_FIXED_SCALE;
        let has_pure_growth = !root.ignore_long_term_rewards && root.effective_has_growth_targets;
        let has_relic = !root.active_relic_targets.is_empty();
        let preserve_theft = root.theft_policy == SolverTheftPolicy::PreserveResources;
        let sig = compute_tactical_signature(
            0,
            k.used_potion_slot_mask,
            self.potion_count(id),
            k.death_save_use_count,
            k.rem_enemies,
            0,
            0,
            k.cumulative_hp_lost as i32,
            0,
            k.best_hp as i32,
            k.enemy_hp,
            0,
            eff_def,
            k.optionality_score,
            0,
            k.setup_score as i32,
            k.board_investment as i32,
            0,
            0,
            has_pure_growth,
            0,
            0,
            0,
            0,
            has_relic,
            0,
            0,
            0,
            0,
            preserve_theft,
            0,
            false,
        );
        ExtendedPruneEntry {
            effective_net_hp_loss_centi: eff_loss,
            tactical_sig: sig,
            node_id: id,
            primary: k.primary,
            aux_score: k.aux_score,
            forced_used_mask: k.forced_used_mask,
            relic_satisfied_mask: 0,
            best_hp: k.best_hp as i32,
            effective_defense: eff_def,
            enemy_hp: k.enemy_hp,
            rem_enemies: k.rem_enemies as i32,
            board_investment: k.board_investment as i32,
            setup_score: k.setup_score as i32,
            max_dark_evoke: 0,
            total_poison_and_doom: 0,
            optionality_score: k.optionality_score,
            strategy_goal_hp_credit: k.strategic_hp_credit as i32,
            relic_distance: 0,
            relic_hp_credit: 0,
            relic_healing_hp_credit: 0,
            relic_satisfied_priority: 0,
            growth_hp_credit: 0,
            growth_reward_count: 0,
            used_potion_slot_mask: k.used_potion_slot_mask,
            potion_cost: k.potion_cost,
            strategic_hp_credit: k.strategic_hp_credit,
            energy: 0,
            outstanding_stolen: 0,
            playable_hand_count: 0,
            optional_paid_potion_count: self.optional_paid_potion_count(id),
            explicit_potion_count: k.explicit_potion_count,
            death_save_use_count: k.death_save_use_count,
            strategy_goal_count: k.strategy_goal_count,
            forfeits_gold: false,
            has_rollout_score: k.has_rollout_score(),
            has_choice_lookahead: k.has_choice_lookahead(),
        }
    }
}

impl PruneSource for NodeArena {
    #[inline(always)]
    fn build_prune_entry(
        &self,
        id: NodeId,
        root: &RootEvalBaseline,
        use_rollout_scores: bool,
    ) -> (PruneKey, i64, i64, i64, i64, f64, crate::hash::StateHash) {
        PruneKey::from_node(self, id, root, use_rollout_scores)
    }

    #[inline(always)]
    fn potion_count(&self, id: NodeId) -> u8 {
        self.get(id).potion_count
    }

    #[inline(always)]
    fn explicit_potion_count(&self, id: NodeId) -> u8 {
        self.get(id).explicit_potion_count
    }

    #[inline(always)]
    fn optional_paid_potion_count(&self, id: NodeId) -> u8 {
        self.get(id).optional_paid_potion_count
    }

    #[inline(always)]
    fn forced_used_mask(&self, id: NodeId) -> u64 {
        self.get(id).forced_used_mask
    }

    #[inline(always)]
    fn node_turn(&self, id: NodeId) -> i32 {
        self.get(id).turn as i32
    }

    #[inline(always)]
    fn beam_score(&self, id: NodeId, use_rollout_scores: bool) -> f64 {
        let n = self.get(id);
        if use_rollout_scores {
            n.effective_turn_boundary_score()
        } else {
            n.beam_rank_score_or_base()
        }
    }

    #[inline(always)]
    fn tie_break_fields(&self, id: NodeId) -> (i16, i32, bool, f64, crate::hash::StateHash) {
        let n = self.get(id);
        (
            n.fast_eval.cumulative_hp_lost,
            n.fast_eval.enemy_hp,
            n.has_choice_lookahead(),
            n.choice_lookahead_score().unwrap_or(f64::NEG_INFINITY),
            n.state_hash,
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
        let n = self.get(id);
        let f = &n.fast_eval;
        let rare = self.rare_eval(id);
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
        let eff_def = if n.has_full_eval() {
            n.effective_defense as i32
        } else {
            f.player_block as i32
        };
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
        let sig = if n.tactical_sig != 0 {
            n.tactical_sig
        } else {
            compute_tactical_signature(
                n.turn,
                n.used_potion_slot_mask,
                n.potion_count,
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
                n.max_dark_evoke as i32,
                n.total_poison_and_doom as i32,
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
            tactical_sig: sig,
            node_id: id,
            primary: k.primary,
            aux_score: k.aux_score,
            forced_used_mask: n.forced_used_mask,
            relic_satisfied_mask: rare.relic_eval.satisfied_mask,
            best_hp: k.best_hp as i32,
            effective_defense: eff_def,
            enemy_hp: f.enemy_hp,
            rem_enemies: k.rem_enemies as i32,
            board_investment: k.board_investment as i32,
            setup_score: k.setup_score as i32,
            max_dark_evoke: n.max_dark_evoke as i32,
            total_poison_and_doom: n.total_poison_and_doom as i32,
            optionality_score: k.optionality_score,
            strategy_goal_hp_credit: f.strategy_goal_hp_credit as i32,
            relic_distance: rare.relic_eval.distance,
            relic_hp_credit: rare.relic_eval.hp_credit,
            relic_healing_hp_credit: rare.relic_eval.healing_hp_credit,
            relic_satisfied_priority: rare.relic_eval.satisfied_priority,
            growth_hp_credit,
            growth_reward_count,
            used_potion_slot_mask: n.used_potion_slot_mask,
            potion_cost: n.potion_strategic_cost,
            strategic_hp_credit: f.strategic_hp_credit,
            energy: f.energy,
            outstanding_stolen: f.outstanding_stolen,
            playable_hand_count: f.playable_hand_count,
            optional_paid_potion_count: n.optional_paid_potion_count,
            explicit_potion_count: n.explicit_potion_count,
            death_save_use_count: f.death_save_use_count,
            strategy_goal_count: f.strategy_goal_count,
            forfeits_gold,
            has_rollout_score: k.has_rollout_score(),
            has_choice_lookahead: k.has_choice_lookahead(),
        }
    }
}

#[derive(Clone, Debug, Default)]
struct BucketPruneScratch {
    nodes: Vec<NodeId>,
    out: Vec<(f64, NodeId)>,
    marks: Vec<u8>,
    unmarked: Vec<usize>,
    heap: Vec<usize>,
    keys: Vec<PruneKey>,
    entries: Vec<ExtendedPruneEntry>,
    state_hashes: Vec<crate::hash::StateHash>,
    node_sigs: Vec<u64>,
    sig_keys: Vec<u64>,
    sig_counts: Vec<u8>,
    heap_sig_keys: Vec<u64>,
    heap_sig_counts: Vec<u8>,
    heap_sig_worst_idx: Vec<i32>,
    selected_indices: Vec<usize>,
    order: Vec<(f64, u32)>,
}

#[derive(Clone, Debug, Default)]
pub struct PruneScratch {
    buckets: [BucketPruneScratch; 4],
    merged: Vec<(f64, NodeId)>,
    pub allow_death_save_rescue: bool,
    pub relax_zero_potion_reserve: bool,
    pub min_zero_potion_reserve_percent: i32,
    pub turn_layer: i32,
}

/// Compile-time full-catalog healing/growth closure certification computed in `< 1 µs` from static metadata.
/// Certifies exact zero-heal / bounded-heal upper bounds on any vanilla combat root, powering sound
/// certified-incumbent HP pruning (`is_pruned_by_certified_incumbent`).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ClosureCertificate {
    pub healing_certified: bool,
    pub max_in_combat_heal: i32,
    pub post_combat_relic_heal: PostCombatRelicHealProfile,
    pub growth_certified_zero: bool,
    pub max_growth_credit: i32,
}

impl ClosureCertificate {
    pub fn certify(state: &BattleState, content: &dyn ContentDispatcher) -> Self {
        let mut healing_certified = true;
        let mut max_in_combat_heal = 0i32;
        let mut growth_certified_zero = true;

        // Check active relics
        for r in &state.player.relics {
            if r.is_melted {
                continue;
            }
            let Ok(meta) = content.relic_meta(r.id) else {
                healing_certified = false;
                growth_certified_zero = false;
                break;
            };
            if meta.flags.contains(RelicStaticFlags::HAS_GROWTH) {
                growth_certified_zero = false;
            }
            if meta.flags.contains(RelicStaticFlags::MAY_HEAL) {
                // BurningBlood, BlackBlood, MeatOnTheBone only heal post-combat;
                // other MAY_HEAL relics may heal during combat so conservatively uncertify.
                if !matches!(
                    r.id,
                    RelicId::BurningBlood | RelicId::BlackBlood | RelicId::MeatOnTheBone
                ) {
                    healing_certified = false;
                }
            }
        }

        // Check potion slots
        if healing_certified || growth_certified_zero {
            for slot in &state.player.potion_slots {
                let Some(pid) = *slot else { continue };
                let Ok(meta) = content.potion_meta(pid) else {
                    healing_certified = false;
                    growth_certified_zero = false;
                    break;
                };
                if meta.may_heal || pid == PotionId::FruitJuice {
                    healing_certified = false;
                }
                // Card/potion generators can produce healing or growth cards dynamically.
                if matches!(
                    pid,
                    PotionId::EntropicBrew
                        | PotionId::AttackPotion
                        | PotionId::ColorlessPotion
                        | PotionId::DistilledChaos
                ) {
                    healing_certified = false;
                    growth_certified_zero = false;
                }
            }
        }

        // Check active creature powers
        if healing_certified || growth_certified_zero {
            for cr in &state.creatures {
                for pw in &cr.powers {
                    let Ok(meta) = content.power_meta(pw.id) else {
                        healing_certified = false;
                        growth_certified_zero = false;
                        break;
                    };
                    if meta.flags.contains(PowerStaticFlags::MAY_HEAL) {
                        healing_certified = false;
                    }
                    if meta.flags.contains(PowerStaticFlags::HAS_GROWTH) {
                        growth_certified_zero = false;
                    }
                }
            }
        }

        // Check live cards across hand/draw/discard/play
        if healing_certified || growth_certified_zero {
            for card in &state.cards {
                if card.removed_from_state || card.pile == PileType::Exhaust {
                    continue;
                }
                let Ok(meta) = content.card_meta(card.id) else {
                    healing_certified = false;
                    growth_certified_zero = false;
                    break;
                };
                if meta.flags.contains(CardStaticFlags::MAY_HEAL) {
                    // Self-contained non-exhausting heal or unknown upper bound
                    healing_certified = false;
                }
                if meta.flags.contains(CardStaticFlags::HAS_GROWTH) {
                    growth_certified_zero = false;
                }
                // Card generation cards can generate healing/growth cards
                if matches!(
                    card.id,
                    CardId::Alchemize
                        | CardId::CreativeAi
                        | CardId::HelloWorld
                        | CardId::Discovery
                        | CardId::WhiteNoise
                        | CardId::Distraction
                        | CardId::InfernalBlade
                ) {
                    healing_certified = false;
                    growth_certified_zero = false;
                }
            }
        }

        if !healing_certified {
            max_in_combat_heal = i32::MAX;
        }

        Self {
            healing_certified,
            max_in_combat_heal,
            post_combat_relic_heal: PostCombatRelicHealProfile::from_state(state),
            growth_certified_zero,
            max_growth_credit: 0,
        }
    }

    #[inline]
    pub fn max_possible_healing_credit(&self, player_hp: i32, player_max_hp: i32) -> i32 {
        if !self.healing_certified {
            return i32::MAX;
        }
        let headroom = (player_max_hp - player_hp).max(0);
        let post_heal =
            self.post_combat_relic_heal.unconditional_heal.max(0) + self.post_combat_relic_heal.wounded_heal.max(0);
        (self.max_in_combat_heal.saturating_add(post_heal)).min(headroom + post_heal)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct PrimaryIncumbentBucket {
    pub potions: u16,
    pub forfeits_gold: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PrimarySearchIncumbent {
    pub strategic_hp_deficit: i32,
    pub combat_ended_turn: i32,
    pub potion_strategic_cost: i32,
}

/// Shared thread-safe incumbent table across waves and portfolio members.
/// Includes an atomic fast-path for zero-potion certified bounds.
pub struct PrimaryIncumbentTable {
    bounds: RwLock<HashMap<PrimaryIncumbentBucket, PrimarySearchIncumbent, ahash::RandomState>>,
    fast_zero_potion_deficit: AtomicI32,
    pub branches_pruned: AtomicI32,
}

impl Default for PrimaryIncumbentTable {
    fn default() -> Self {
        Self::new()
    }
}

impl PrimaryIncumbentTable {
    pub fn new() -> Self {
        Self {
            bounds: RwLock::new(HashMap::with_hasher(ahash::RandomState::with_seeds(
                11, 22, 33, 44,
            ))),
            fast_zero_potion_deficit: AtomicI32::new(i32::MAX),
            branches_pruned: AtomicI32::new(0),
        }
    }

    #[inline]
    pub fn fast_zero_potion_bound(&self) -> i32 {
        self.fast_zero_potion_deficit.load(Ordering::Relaxed)
    }

    pub fn tighten(&self, bucket: PrimaryIncumbentBucket, candidate: PrimarySearchIncumbent) -> bool {
        let mut w = self.bounds.write();
        if let Some(cur) = w.get(&bucket) {
            if candidate.strategic_hp_deficit > cur.strategic_hp_deficit
                || (candidate.strategic_hp_deficit == cur.strategic_hp_deficit
                    && candidate.combat_ended_turn >= cur.combat_ended_turn)
            {
                return false;
            }
        }
        w.insert(bucket, candidate);
        if bucket.potions == 0 && !bucket.forfeits_gold {
            self.fast_zero_potion_deficit
                .fetch_min(candidate.strategic_hp_deficit, Ordering::Relaxed);
        }
        true
    }

    pub fn try_get(&self, bucket: PrimaryIncumbentBucket) -> Option<PrimarySearchIncumbent> {
        self.bounds.read().get(&bucket).copied()
    }
}

/// Wave-frozen certified centi-HP incumbent bound matching `HotIncumbentBound` (`HotEvaluator.cs`).
/// Snapshotted once before each wave so DOP=1 and DOP=N make identical pruning decisions while
/// preserving `(V, NetPersistentHpDeltaCenti, Turn)` tie-break priority.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FrozenIncumbentBound {
    pub has_certified_zero_potion_bound: bool,
    pub best_zero_potion_hp_delta_centi: i32,
    pub best_zero_potion_battle_hp_lost: i32,
    pub best_zero_potion_end_turn: i32,
    pub has_certified_overall_bound: bool,
    pub best_overall_value_centi: i64,
    pub best_overall_hp_delta_centi: i32,
    pub best_overall_battle_hp_lost: i32,
    pub best_overall_potion_count: usize,
    pub best_overall_end_turn: i32,
}

impl FrozenIncumbentBound {
    #[allow(clippy::too_many_arguments)]
    #[inline]
    pub fn tighten(
        &mut self,
        root: &RootEvalBaseline,
        won: bool,
        death_save_use_count: i32,
        net_hp_delta_centi: i32,
        net_potion_cost_centi: i32,
        battle_hp_lost: i32,
        potion_count: usize,
        optional_paid_count: usize,
        explicit_potion_count: usize,
        end_turn: i32,
        _outstanding_stolen: i32,
        _forfeits_gold: bool,
        theft_preserve_resources: bool,
    ) -> bool {
        if !won
            || root.has_any_heal_or_unknown_generation
            || root.effective_has_growth_targets
            || !root.effective_growth_budgets.is_zero()
            || !root.active_relic_targets.is_empty()
            || root.all_forced_potion_mask != 0
            || theft_preserve_resources
            || death_save_use_count > 0
        {
            return false;
        }
        let mut changed = false;
        if optional_paid_count == 0 && explicit_potion_count == 0 {
            if !self.has_certified_zero_potion_bound
                || net_hp_delta_centi > self.best_zero_potion_hp_delta_centi
                || (net_hp_delta_centi == self.best_zero_potion_hp_delta_centi
                    && (battle_hp_lost < self.best_zero_potion_battle_hp_lost
                        || (battle_hp_lost == self.best_zero_potion_battle_hp_lost
                            && end_turn < self.best_zero_potion_end_turn)))
            {
                self.has_certified_zero_potion_bound = true;
                self.best_zero_potion_hp_delta_centi = net_hp_delta_centi;
                self.best_zero_potion_battle_hp_lost = battle_hp_lost;
                self.best_zero_potion_end_turn = end_turn;
                changed = true;
            }
        }
        let v_centi = (net_hp_delta_centi as i64) - (net_potion_cost_centi as i64);
        if !self.has_certified_overall_bound
            || v_centi > self.best_overall_value_centi
            || (v_centi == self.best_overall_value_centi
                && net_hp_delta_centi > self.best_overall_hp_delta_centi)
            || (v_centi == self.best_overall_value_centi
                && net_hp_delta_centi == self.best_overall_hp_delta_centi
                && (battle_hp_lost < self.best_overall_battle_hp_lost
                    || (battle_hp_lost == self.best_overall_battle_hp_lost
                        && (potion_count < self.best_overall_potion_count
                            || (potion_count == self.best_overall_potion_count
                                && end_turn < self.best_overall_end_turn)))))
        {
            self.has_certified_overall_bound = true;
            self.best_overall_value_centi = v_centi;
            self.best_overall_hp_delta_centi = net_hp_delta_centi;
            self.best_overall_battle_hp_lost = battle_hp_lost;
            self.best_overall_potion_count = potion_count;
            self.best_overall_end_turn = end_turn;
            changed = true;
        }
        changed
    }

    #[allow(clippy::too_many_arguments)]
    #[inline]
    pub fn is_pruned(
        &self,
        cert: &ClosureCertificate,
        root: &RootEvalBaseline,
        player_hp: i32,
        player_max_hp: i32,
        death_save_hp_restored: i32,
        death_save_use_count: i32,
        feed_count: u8,
        won: bool,
        player_dead: bool,
        turn: i32,
        battle_hp_lost: i32,
        potion_count: usize,
        optional_paid_count: usize,
        explicit_potion_count: usize,
        net_potion_cost_centi: i32,
        theft_preserve_resources: bool,
    ) -> bool {
        if !cert.healing_certified
            || !cert.growth_certified_zero
            || root.has_any_heal_or_unknown_generation
            || root.effective_has_growth_targets
            || !root.effective_growth_budgets.is_zero()
            || !root.active_relic_targets.is_empty()
            || root.all_forced_potion_mask != 0
            || theft_preserve_resources
            || player_dead
        {
            return false;
        }
        let max_post_combat_heal = root
            .post_combat_relic_heal
            .monotone_heal_for(player_hp, player_max_hp);
        let upper_bound_hp_delta_centi = ActEndingBossPolicy::compute_net_persistent_hp_delta_centi(
            player_hp,
            max_post_combat_heal,
            root.initial_player_hp,
            player_max_hp,
            root.initial_player_max_hp,
            death_save_hp_restored,
            0,
            root.boss_hp_relief,
            root.ignore_long_term_rewards,
            i32::from(feed_count) * root.feed_max_hp_per_kill,
        );

        if optional_paid_count == 0
            && explicit_potion_count == 0
            && self.has_certified_zero_potion_bound
        {
            if death_save_use_count > 0 {
                return true;
            }
            if upper_bound_hp_delta_centi < self.best_zero_potion_hp_delta_centi {
                return true;
            }
            if upper_bound_hp_delta_centi == self.best_zero_potion_hp_delta_centi
                && !won
                && battle_hp_lost >= self.best_zero_potion_battle_hp_lost
                && turn > self.best_zero_potion_end_turn
            {
                return true;
            }
        }

        if self.has_certified_overall_bound {
            if death_save_use_count > 0 {
                return true;
            }
            let upper_bound_value_centi =
                (upper_bound_hp_delta_centi as i64) - (net_potion_cost_centi as i64);
            if upper_bound_value_centi < self.best_overall_value_centi {
                return true;
            }
            if upper_bound_value_centi == self.best_overall_value_centi {
                if upper_bound_hp_delta_centi < self.best_overall_hp_delta_centi {
                    return true;
                }
                if upper_bound_hp_delta_centi == self.best_overall_hp_delta_centi
                    && !won
                    && battle_hp_lost >= self.best_overall_battle_hp_lost
                    && (battle_hp_lost > self.best_overall_battle_hp_lost
                        || potion_count > self.best_overall_potion_count
                        || (potion_count == self.best_overall_potion_count
                            && turn > self.best_overall_end_turn))
                {
                    return true;
                }
            }
        }

        false
    }
}

/// Sound certified-incumbent HP lower-bound pruning.
/// Returns `true` when `node` cannot possibly beat the already witnessed victory under `ClosureCertificate`.
#[allow(clippy::too_many_arguments)]
#[inline]
pub fn is_pruned_by_certified_incumbent(
    cert: &ClosureCertificate,
    root: &RootEvalBaseline,
    table: &PrimaryIncumbentTable,
    cumulative_hp_lost: i32,
    player_hp: i32,
    player_max_hp: i32,
    initial_player_max_hp: i32,
    death_save_hp_restored: i32,
    death_save_use_count: i32,
    potions_used: u16,
    turn: i32,
    relief: BossHpRelief,
) -> bool {
    if !cert.healing_certified
        || !cert.growth_certified_zero
        || death_save_use_count > 0
        || root.all_forced_potion_mask != 0
        || root.effective_has_growth_targets
        || !root.effective_growth_budgets.is_zero()
    {
        return false;
    }
    let has_relic_targets = !root.active_relic_targets.is_empty();
    if has_relic_targets
        && root
            .active_relic_targets
            .iter()
            .any(|t| t.hp_allowance > 0 || t.id == RelicCounterId::MeatOnTheBone)
    {
        return false;
    }
    let max_heal = cert.max_possible_healing_credit(player_hp, player_max_hp);
    if max_heal == i32::MAX {
        return false;
    }
    let max_hp_deficit = (initial_player_max_hp - player_max_hp).max(0);
    let hp_lower_bound = ActEndingBossPolicy::strategic_hp_deficit(
        cumulative_hp_lost,
        max_hp_deficit,
        max_heal,
        relief,
        death_save_hp_restored,
    );

    if potions_used == 0 && !has_relic_targets {
        let bound = table.fast_zero_potion_bound();
        if bound < i32::MAX && hp_lower_bound > bound {
            table.branches_pruned.fetch_add(1, Ordering::Relaxed);
            return true;
        }
    }

    if let Some(inc) = table.try_get(PrimaryIncumbentBucket {
        potions: potions_used,
        forfeits_gold: false,
    }) {
        if hp_lower_bound > inc.strategic_hp_deficit
            || (!has_relic_targets
                && hp_lower_bound == inc.strategic_hp_deficit
                && turn > inc.combat_ended_turn)
        {
            table.branches_pruned.fetch_add(1, Ordering::Relaxed);
            return true;
        }
    }
    false
}

/// Multi-lane Beam Retention Policy matching `CombatBeamSolver.BeamRetentionPolicy.cs` + `.Ranking.cs`.
pub struct BeamRetentionPolicy;

impl BeamRetentionPolicy {
    #[inline]
    pub fn feasible_potion_use_quotas(limit: usize) -> (usize, usize) {
        if limit < 4 {
            (1, limit.saturating_sub(1))
        } else {
            let used = (limit / 3).max(2);
            (used, limit.saturating_sub(used))
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn prune_wave(
        arena: &mut NodeArena,
        candidates: &[NodeId],
        beam_width: usize,
        potion_free_beam_width: Option<usize>,
        potion_beam_width: Option<usize>,
        root: &RootEvalBaseline,
        second_rank_band: bool,
        aggressive_power_commitment: bool,
    ) -> Vec<NodeId> {
        Self::prune_wave_with_potion_bias(
            arena,
            candidates,
            beam_width,
            potion_free_beam_width,
            potion_beam_width,
            root,
            second_rank_band,
            aggressive_power_commitment,
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn prune_wave_with_potion_bias(
        arena: &mut NodeArena,
        candidates: &[NodeId],
        beam_width: usize,
        potion_free_beam_width: Option<usize>,
        potion_beam_width: Option<usize>,
        root: &RootEvalBaseline,
        second_rank_band: bool,
        aggressive_power_commitment: bool,
        prefer_potion_buckets: bool,
    ) -> Vec<NodeId> {
        Self::prune_wave_full(
            arena,
            candidates,
            beam_width,
            potion_free_beam_width,
            potion_beam_width,
            root,
            second_rank_band,
            aggressive_power_commitment,
            prefer_potion_buckets,
            false,
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn prune_wave_with_potion_bias_into(
        arena: &NodeArena,
        candidates: &[NodeId],
        beam_width: usize,
        potion_free_beam_width: Option<usize>,
        potion_beam_width: Option<usize>,
        root: &RootEvalBaseline,
        second_rank_band: bool,
        aggressive_power_commitment: bool,
        prefer_potion_buckets: bool,
        pool: Option<&rayon::ThreadPool>,
        out: &mut Vec<NodeId>,
        scratch: &mut PruneScratch,
    ) {
        Self::prune_wave_full_into(
            arena,
            candidates,
            beam_width,
            potion_free_beam_width,
            potion_beam_width,
            root,
            second_rank_band,
            aggressive_power_commitment,
            prefer_potion_buckets,
            false,
            false,
            pool,
            out,
            scratch,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn prune_wave_with_rollout(
        arena: &mut NodeArena,
        candidates: &[NodeId],
        beam_width: usize,
        potion_free_beam_width: Option<usize>,
        potion_beam_width: Option<usize>,
        root: &RootEvalBaseline,
        second_rank_band: bool,
        aggressive_power_commitment: bool,
        prefer_potion_buckets: bool,
        use_rollout_scores: bool,
    ) -> Vec<NodeId> {
        Self::prune_wave_full(
            arena,
            candidates,
            beam_width,
            potion_free_beam_width,
            potion_beam_width,
            root,
            second_rank_band,
            aggressive_power_commitment,
            prefer_potion_buckets,
            true,
            use_rollout_scores,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn prune_wave_with_rollout_into(
        arena: &NodeArena,
        candidates: &[NodeId],
        beam_width: usize,
        potion_free_beam_width: Option<usize>,
        potion_beam_width: Option<usize>,
        root: &RootEvalBaseline,
        second_rank_band: bool,
        aggressive_power_commitment: bool,
        prefer_potion_buckets: bool,
        use_rollout_scores: bool,
        pool: Option<&rayon::ThreadPool>,
        out: &mut Vec<NodeId>,
        scratch: &mut PruneScratch,
    ) {
        Self::prune_wave_full_into(
            arena,
            candidates,
            beam_width,
            potion_free_beam_width,
            potion_beam_width,
            root,
            second_rank_band,
            aggressive_power_commitment,
            prefer_potion_buckets,
            true,
            use_rollout_scores,
            pool,
            out,
            scratch,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn prune_wave_full(
        arena: &mut NodeArena,
        candidates: &[NodeId],
        beam_width: usize,
        potion_free_beam_width: Option<usize>,
        potion_beam_width: Option<usize>,
        root: &RootEvalBaseline,
        second_rank_band: bool,
        aggressive_power_commitment: bool,
        prefer_potion_buckets: bool,
        is_turn_boundary: bool,
        use_rollout_scores: bool,
    ) -> Vec<NodeId> {
        let mut scratch = PruneScratch::default();
        let mut out = Vec::new();
        Self::prune_wave_full_into(
            arena,
            candidates,
            beam_width,
            potion_free_beam_width,
            potion_beam_width,
            root,
            second_rank_band,
            aggressive_power_commitment,
            prefer_potion_buckets,
            is_turn_boundary,
            use_rollout_scores,
            None,
            &mut out,
            &mut scratch,
        );
        out
    }

    #[allow(clippy::too_many_arguments)]
    pub fn prune_wave_full_into(
        arena: &NodeArena,
        candidates: &[NodeId],
        beam_width: usize,
        potion_free_beam_width: Option<usize>,
        potion_beam_width: Option<usize>,
        root: &RootEvalBaseline,
        second_rank_band: bool,
        aggressive_power_commitment: bool,
        prefer_potion_buckets: bool,
        is_turn_boundary: bool,
        use_rollout_scores: bool,
        pool: Option<&rayon::ThreadPool>,
        selected: &mut Vec<NodeId>,
        scratch: &mut PruneScratch,
    ) {
        Self::prune_with_source_into(
            arena,
            candidates,
            beam_width,
            potion_free_beam_width,
            potion_beam_width,
            root,
            second_rank_band,
            aggressive_power_commitment,
            prefer_potion_buckets,
            is_turn_boundary,
            use_rollout_scores,
            pool,
            selected,
            scratch,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prune_with_source_into<S: PruneSource>(
        source: &S,
        candidates: &[NodeId],
        beam_width: usize,
        potion_free_beam_width: Option<usize>,
        potion_beam_width: Option<usize>,
        root: &RootEvalBaseline,
        second_rank_band: bool,
        aggressive_power_commitment: bool,
        prefer_potion_buckets: bool,
        is_turn_boundary: bool,
        use_rollout_scores: bool,
        pool: Option<&rayon::ThreadPool>,
        selected: &mut Vec<NodeId>,
        scratch: &mut PruneScratch,
    ) {
        selected.clear();
        if candidates.is_empty() || beam_width == 0 {
            return;
        }
        let has_explicit_split = potion_free_beam_width.is_some() || potion_beam_width.is_some();
        if candidates.len() <= beam_width && !has_explicit_split {
            selected.extend_from_slice(candidates);
            selected.sort_unstable_by(|&a, &b| cmp_primary_desc(source, a, b, use_rollout_scores));
            return;
        }

        let effective_turn_layer = if scratch.turn_layer > 0 {
            scratch.turn_layer
        } else if root.start_turn_number > 0 {
            let mut max_t = 0i32;
            for &nid in candidates {
                max_t = max_t.max(source.node_turn(nid));
            }
            if is_turn_boundary {
                (max_t - root.start_turn_number - 1).max(0)
            } else {
                (max_t - root.start_turn_number).max(0)
            }
        } else {
            0
        };

        let PruneScratch {
            buckets,
            merged,
            allow_death_save_rescue,
            relax_zero_potion_reserve,
            min_zero_potion_reserve_percent,
            turn_layer: _,
        } = scratch;
        let allow_death_save_rescue = *allow_death_save_rescue;

        // Single-pass 4-bucket partitioning by policy-aware optional paid + forced potion count.
        let mut counts = [0usize; 4];
        for b in 0..4 {
            buckets[b].nodes.clear();
            buckets[b].out.clear();
        }
        for &nid in candidates {
            let b = compute_potion_bucket(
                source.potion_count(nid),
                source.explicit_potion_count(nid),
                source.optional_paid_potion_count(nid),
                source.forced_used_mask(nid),
                root,
                has_explicit_split,
            );
            counts[b] += 1;
            buckets[b].nodes.push(nid);
        }
        let has_forced_directives =
            root.forced_directive_count > 0 || root.all_forced_potion_mask != 0;
        let bucket_quotas = if has_explicit_split {
            allocate_explicit_split_quotas(
                counts,
                beam_width,
                potion_free_beam_width,
                potion_beam_width,
                prefer_potion_buckets,
                *relax_zero_potion_reserve,
                *min_zero_potion_reserve_percent,
                has_forced_directives,
                effective_turn_layer,
            )
        } else {
            allocate_potion_bucket_quotas(
                counts,
                beam_width,
                prefer_potion_buckets,
                *relax_zero_potion_reserve,
                *min_zero_potion_reserve_percent,
                has_forced_directives,
                effective_turn_layer,
            )
        };
        let active_buckets = (0..4)
            .filter(|&b| !buckets[b].nodes.is_empty() && bucket_quotas[b] > 0)
            .count();

        if let Some(pool) = pool.filter(|_| active_buckets >= 2 && candidates.len() >= 128) {
            let largest_b = (0..4)
                .filter(|&b| !buckets[b].nodes.is_empty() && bucket_quotas[b] > 0)
                .max_by_key(|&b| buckets[b].nodes.len())
                .unwrap_or(0);
            pool.scope(|scope| {
                let mut largest_bucket: Option<(usize, &mut BucketPruneScratch)> = None;
                for (b, bucket) in buckets.iter_mut().enumerate() {
                    let quota = bucket_quotas[b];
                    if bucket.nodes.is_empty() || quota == 0 {
                        continue;
                    }
                    if b == largest_b {
                        largest_bucket = Some((quota, bucket));
                        continue;
                    }
                    scope.spawn(move |_| {
                        select_from_potion_bucket_scratch(
                            source,
                            bucket,
                            quota,
                            b,
                            root,
                            second_rank_band,
                            aggressive_power_commitment,
                            prefer_potion_buckets,
                            is_turn_boundary,
                            use_rollout_scores,
                            allow_death_save_rescue,
                            Some(pool),
                        );
                    });
                }
                if let Some((quota, bucket)) = largest_bucket {
                    select_from_potion_bucket_scratch(
                        source,
                        bucket,
                        quota,
                        largest_b,
                        root,
                        second_rank_band,
                        aggressive_power_commitment,
                        prefer_potion_buckets,
                        is_turn_boundary,
                        use_rollout_scores,
                        allow_death_save_rescue,
                        Some(pool),
                    );
                }
            });
        } else {
            for (b, bucket) in buckets.iter_mut().enumerate() {
                let quota = bucket_quotas[b];
                if bucket.nodes.is_empty() || quota == 0 {
                    continue;
                }
                let single_pool = if active_buckets == 1 { pool } else { None };
                select_from_potion_bucket_scratch(
                    source,
                    bucket,
                    quota,
                    b,
                    root,
                    second_rank_band,
                    aggressive_power_commitment,
                    prefer_potion_buckets,
                    is_turn_boundary,
                    use_rollout_scores,
                    allow_death_save_rescue,
                    single_pool,
                );
            }
        }

        merged.clear();
        merged.reserve(beam_width);
        for bucket in buckets.iter() {
            merged.extend_from_slice(&bucket.out);
        }
        // Deterministic canonical order across buckets so subsequent waves are DOP-invariant.
        merged.sort_unstable_by(|&(sa, a), &(sb, b)| {
            sb.total_cmp(&sa)
                .then_with(|| tie_break_deterministic(source, a, b))
        });
        selected.reserve(merged.len());
        for &(_score, nid) in merged.iter() {
            #[cfg(debug_assertions)]
            debug_assert_eq!(
                _score.to_bits(),
                source.beam_score(nid, use_rollout_scores).to_bits()
            );
            selected.push(nid);
        }
    }
}

#[inline]
fn node_beam_score<S: PruneSource>(source: &S, id: NodeId, use_rollout_scores: bool) -> f64 {
    source.beam_score(id, use_rollout_scores)
}

#[inline]
fn tie_break_deterministic<S: PruneSource>(source: &S, a: NodeId, b: NodeId) -> std::cmp::Ordering {
    let (a_chl, a_ehp, a_hcla, a_cla, a_hash) = source.tie_break_fields(a);
    let (b_chl, b_ehp, b_hcla, b_cla, b_hash) = source.tie_break_fields(b);
    a_chl
        .cmp(&b_chl)
        .then_with(|| a_ehp.cmp(&b_ehp))
        .then_with(|| {
            b_hcla.cmp(&a_hcla).then_with(|| {
                if a_hcla && b_hcla {
                    b_cla.total_cmp(&a_cla)
                } else {
                    std::cmp::Ordering::Equal
                }
            })
        })
        .then_with(|| a_hash.cmp(&b_hash))
        .then_with(|| a.0.cmp(&b.0))
}

#[inline(always)]
fn cmp_key_tie_break(
    keys: &[PruneKey],
    state_hashes: &[crate::hash::StateHash],
    ia: usize,
    ib: usize,
) -> std::cmp::Ordering {
    let a = &keys[ia];
    let b = &keys[ib];
    a.cumulative_hp_lost
        .cmp(&b.cumulative_hp_lost)
        .then_with(|| a.enemy_hp.cmp(&b.enemy_hp))
        .then_with(|| {
            let ha = a.has_choice_lookahead();
            let hb = b.has_choice_lookahead();
            hb.cmp(&ha).then_with(|| {
                if ha && hb {
                    b.aux_score.total_cmp(&a.aux_score)
                } else {
                    std::cmp::Ordering::Equal
                }
            })
        })
        .then_with(|| state_hashes[ia].cmp(&state_hashes[ib]))
        .then_with(|| a.node_id.0.cmp(&b.node_id.0))
}

#[cfg(test)]
#[inline(always)]
fn cmp_key_primary_desc(
    keys: &[PruneKey],
    state_hashes: &[crate::hash::StateHash],
    ia: usize,
    ib: usize,
) -> std::cmp::Ordering {
    keys[ib]
        .primary
        .total_cmp(&keys[ia].primary)
        .then_with(|| cmp_key_tie_break(keys, state_hashes, ia, ib))
}

#[inline]
fn compute_niche_quotas(
    rem: usize,
    is_turn_boundary: bool,
    aggressive_power_commitment: bool,
) -> (usize, usize, usize, usize, usize, usize) {
    match rem {
        0 => (0, 0, 0, 0, 0, 0),
        1 => (1, 0, 0, 0, 0, 0),
        2 => (1, 1, 0, 0, 0, 0),
        3 => (1, 1, 0, 1, 0, 0),
        4 => (1, 1, 1, 1, 0, 0),
        5 => (1, 1, 1, 1, 0, 1),
        _ => {
            if is_turn_boundary {
                let survival = ((rem * 16) / 100).max(1);
                let tempo = ((rem * 13) / 100).max(1);
                let inv_pct = if aggressive_power_commitment { 22 } else { 16 };
                let investment = ((rem * inv_pct) / 100)
                    .max(if aggressive_power_commitment { 2 } else { 1 });
                let niche5_total = ((rem * 12) / 100).max(1);
                let optionality = if niche5_total >= 2 {
                    ((niche5_total * 35 + 50) / 100).max(1)
                } else {
                    0
                };
                let pareto = niche5_total.saturating_sub(optionality);
                let used = survival + tempo + investment + niche5_total;
                if used < rem {
                    (rem - used, survival, tempo, investment, optionality, pareto)
                } else {
                    let primary = 1;
                    let mut rest = rem - 1;
                    let s = rest.min(survival).max(usize::from(rest > 0));
                    rest -= s;
                    let inv = rest.min(investment);
                    rest -= inv;
                    let t = rest.min(tempo);
                    rest -= t;
                    let o = rest.min(optionality);
                    rest -= o;
                    let pa = rest.min(pareto);
                    (primary, s, t, inv, o, pa)
                }
            } else {
                let survival = ((rem * 14) / 100).max(1);
                let tempo = ((rem * 11) / 100).max(1);
                let inv_pct = if aggressive_power_commitment { 20 } else { 14 };
                let investment = ((rem * inv_pct) / 100)
                    .max(if aggressive_power_commitment { 2 } else { 1 });
                let optionality = ((rem * 10 + 99) / 100).max((rem * 12) / 100);
                let pareto = if rem >= 10 {
                    ((rem * 7) / 100).max(1)
                } else {
                    0
                };
                let used = survival + tempo + investment + optionality + pareto;
                if used < rem {
                    (rem - used, survival, tempo, investment, optionality, pareto)
                } else {
                    let primary = 1;
                    let mut rest = rem - 1;
                    let s = rest.min(survival).max(usize::from(rest > 0));
                    rest -= s;
                    let inv = rest.min(investment);
                    rest -= inv;
                    let t = rest.min(tempo);
                    rest -= t;
                    let o = rest.min(optionality);
                    rest -= o;
                    let pa = rest.min(pareto);
                    (primary, s, t, inv, o, pa)
                }
            }
        }
    }
}

#[inline]
fn cmp_primary_desc<S: PruneSource>(
    source: &S,
    a: NodeId,
    b: NodeId,
    use_rollout_scores: bool,
) -> std::cmp::Ordering {
    let sa = node_beam_score(source, a, use_rollout_scores);
    let sb = node_beam_score(source, b, use_rollout_scores);
    sb.total_cmp(&sa)
        .then_with(|| tie_break_deterministic(source, a, b))
}

#[allow(clippy::too_many_arguments)]
fn allocate_potion_bucket_quotas(
    counts: [usize; 4],
    total_quota: usize,
    prefer_potion_buckets: bool,
    relax_zero_potion_reserve: bool,
    min_zero_potion_reserve_percent: i32,
    has_forced_directives: bool,
    turn_layer: i32,
) -> [usize; 4] {
    let mut quotas = [0usize; 4];
    let active_buckets = counts.iter().filter(|&&c| c > 0).count();
    if active_buckets == 0 || total_quota == 0 {
        return quotas;
    }
    if active_buckets == 1 {
        for b in 0..4 {
            if counts[b] > 0 {
                quotas[b] = counts[b].min(total_quota);
                return quotas;
            }
        }
    }

    let mut remaining = total_quota;
    if counts[0] > 0 {
        let mut enforce_ceil_floor = false;
        let zero_pct: usize = if has_forced_directives {
            if !prefer_potion_buckets {
                if turn_layer <= 1 { 25 } else { 15 }
            } else if turn_layer <= 1 {
                20
            } else {
                12
            }
        } else if !prefer_potion_buckets {
            60
        } else {
            let mut pct = if turn_layer <= 1 {
                if relax_zero_potion_reserve { 25 } else { 30 }
            } else if relax_zero_potion_reserve {
                25
            } else {
                26
            };
            let floor_pct = min_zero_potion_reserve_percent.max(25).clamp(25, 60);
            if pct < floor_pct {
                pct = floor_pct;
            }
            enforce_ceil_floor = true;
            pct as usize
        };
        let mut zero_target = if enforce_ceil_floor {
            ((total_quota * zero_pct + 99) / 100).max(1)
        } else {
            ((total_quota * zero_pct) / 100).max(1)
        };
        if enforce_ceil_floor && total_quota > 1 && active_buckets > 1 && zero_target >= total_quota
        {
            zero_target = total_quota - 1;
        }
        let take0 = counts[0].min(zero_target);
        quotas[0] = take0;
        remaining -= take0;
    }

    let mut other_buckets = counts[1..4].iter().filter(|&&c| c > 0).count();
    if other_buckets > 0 && remaining > 0 {
        if !prefer_potion_buckets {
            const PHASE_A_WEIGHTS: [usize; 4] = [0, 55, 30, 15];
            let weight_sum: usize = (1..4)
                .filter(|&b| counts[b] > 0)
                .map(|b| PHASE_A_WEIGHTS[b])
                .sum();
            let budget_for_other = remaining;
            for b in 1..4 {
                if remaining == 0 {
                    break;
                }
                if counts[b] == 0 {
                    continue;
                }
                let target = if other_buckets == 1 {
                    remaining
                } else {
                    ((budget_for_other * PHASE_A_WEIGHTS[b]) / weight_sum).max(1)
                };
                let take = counts[b].min(target.min(remaining));
                quotas[b] = take;
                remaining -= take;
            }
        } else {
            if counts[1] > 0 && other_buckets > 1 {
                let one_pot_pct = if turn_layer <= 1 && counts[2] > 0 {
                    45
                } else {
                    55
                };
                let one_target = ((remaining * one_pot_pct) / 100).max(1);
                let take1 = counts[1].min(one_target);
                quotas[1] = take1;
                remaining -= take1;
                other_buckets -= 1;
            }
            if other_buckets > 0 && remaining > 0 {
                let share = (remaining / other_buckets).max(1);
                for b in 1..4 {
                    if remaining == 0 {
                        break;
                    }
                    if counts[b] == 0 || quotas[b] > 0 {
                        continue;
                    }
                    let take = counts[b].min(share.min(remaining));
                    quotas[b] = take;
                    remaining -= take;
                }
            }
        }
    }

    let start = if prefer_potion_buckets || has_forced_directives {
        1
    } else {
        0
    };
    while remaining > 0 {
        let mut progressed = false;
        for step in 0..4 {
            if remaining == 0 {
                break;
            }
            let b = (start + step) % 4;
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
    quotas
}

#[allow(clippy::too_many_arguments)]
fn allocate_explicit_split_quotas(
    counts: [usize; 4],
    beam_width: usize,
    potion_free_beam_width: Option<usize>,
    potion_beam_width: Option<usize>,
    prefer_potion_buckets: bool,
    relax_zero_potion_reserve: bool,
    min_zero_potion_reserve_percent: i32,
    has_forced_directives: bool,
    turn_layer: i32,
) -> [usize; 4] {
    let max_free = potion_free_beam_width
        .unwrap_or(beam_width)
        .min(beam_width)
        .min(counts[0]);
    let used_avail: usize = counts[1..4].iter().sum();
    let max_used = potion_beam_width
        .unwrap_or(beam_width)
        .min(beam_width)
        .min(used_avail);

    let (q0, q_used) = match (potion_free_beam_width, potion_beam_width) {
        (Some(_), Some(_)) if max_free + max_used <= beam_width => (max_free, max_used),
        (Some(fw), Some(uw)) => {
            let denom = (fw + uw).max(1);
            let mut f = ((beam_width * fw) / denom).min(max_free);
            let mut u = (beam_width - f).min(max_used);
            let mut rem = beam_width.saturating_sub(f + u);
            let add_f = rem.min(max_free.saturating_sub(f));
            f += add_f;
            rem -= add_f;
            let add_u = rem.min(max_used.saturating_sub(u));
            u += add_u;
            (f, u)
        }
        (Some(_), None) => (max_free, (beam_width.saturating_sub(max_free)).min(max_used)),
        (None, Some(_)) => ((beam_width.saturating_sub(max_used)).min(max_free), max_used),
        (None, None) => unreachable!(),
    };

    let mut quotas = allocate_potion_bucket_quotas(
        [0, counts[1], counts[2], counts[3]],
        q_used,
        prefer_potion_buckets,
        relax_zero_potion_reserve,
        min_zero_potion_reserve_percent,
        has_forced_directives,
        turn_layer,
    );
    quotas[0] = q0;
    quotas
}

// Fast O(N) SoA 2-phase bounded heap top-K selection (retained for unit tests).
#[cfg(test)]
#[inline]
#[allow(clippy::too_many_arguments)]
fn pick_top_k_niche_fast_impl(
    keys: &[PruneKey],
    state_hashes: &[crate::hash::StateHash],
    fast_keys: &[i64],
    marks: &mut [u8],
    heap_fast: &mut Vec<(i64, f64, u32)>,
    take: usize,
    pareto_extra: usize,
    out: &mut Vec<(f64, NodeId)>,
    pareto_out: &mut [usize; PER_NICHE_PARETO_TOP_K],
    pareto_count_out: &mut usize,
) -> usize {
    *pareto_count_out = 0;
    let total_k = take + pareto_extra;
    if total_k == 0 {
        return 0;
    }
    #[inline(always)]
    fn cmp_fast_entry(
        fa: i64,
        pa: f64,
        ia: u32,
        fb: i64,
        pb: f64,
        ib: u32,
        keys: &[PruneKey],
        state_hashes: &[crate::hash::StateHash],
    ) -> std::cmp::Ordering {
        if fa != fb {
            fb.cmp(&fa)
        } else if pa > pb {
            std::cmp::Ordering::Less
        } else if pa < pb {
            std::cmp::Ordering::Greater
        } else {
            cmp_key_primary_desc(keys, state_hashes, ia as usize, ib as usize)
        }
    }
    #[inline(always)]
    fn maybe_push_fast(
        i: usize,
        k_fast: i64,
        keys: &[PruneKey],
        state_hashes: &[crate::hash::StateHash],
        heap_fast: &mut Vec<(i64, f64, u32)>,
        w_fast: &mut i64,
        w_primary: &mut f64,
        w_idx: &mut u32,
    ) {
        let k_primary = keys[i].primary;
        let reject = k_fast == *w_fast
            && (k_primary < *w_primary
                || (!(k_primary > *w_primary)
                    && !cmp_key_primary_desc(keys, state_hashes, i, *w_idx as usize).is_lt()));
        if !reject {
            let i_u32 = i as u32;
            let mut pos = 0;
            let len = heap_fast.len();
            loop {
                let left = (pos << 1) + 1;
                if left >= len {
                    break;
                }
                let right = left + 1;
                let (l_fast, l_primary, l_idx) = heap_fast[left];
                let (worst_child, c_fast, c_primary, c_idx) = if right < len {
                    let (r_fast, r_primary, r_idx) = heap_fast[right];
                    if cmp_fast_entry(
                        l_fast,
                        l_primary,
                        l_idx,
                        r_fast,
                        r_primary,
                        r_idx,
                        keys,
                        state_hashes,
                    )
                    .is_lt()
                    {
                        (right, r_fast, r_primary, r_idx)
                    } else {
                        (left, l_fast, l_primary, l_idx)
                    }
                } else {
                    (left, l_fast, l_primary, l_idx)
                };
                if cmp_fast_entry(
                    k_fast,
                    k_primary,
                    i_u32,
                    c_fast,
                    c_primary,
                    c_idx,
                    keys,
                    state_hashes,
                )
                .is_lt()
                {
                    heap_fast[pos] = (c_fast, c_primary, c_idx);
                    pos = worst_child;
                } else {
                    break;
                }
            }
            heap_fast[pos] = (k_fast, k_primary, i_u32);
            (*w_fast, *w_primary, *w_idx) = heap_fast[0];
        }
    }
    heap_fast.clear();
    let n = keys.len().min(fast_keys.len());
    let mut w_fast = 0i64;
    let mut w_primary = 0.0f64;
    let mut w_idx = 0u32;
    let mut i = 0usize;
    while i < n && heap_fast.len() < total_k {
        if marks[i] == 0 {
            let k_fast = fast_keys[i];
            let k_primary = keys[i].primary;
            let i_u32 = i as u32;
            let mut pos = heap_fast.len();
            heap_fast.push((k_fast, k_primary, i_u32));
            while pos > 0 {
                let parent = (pos - 1) >> 1;
                let (p_fast, p_primary, p_idx) = heap_fast[parent];
                if cmp_fast_entry(
                    p_fast,
                    p_primary,
                    p_idx,
                    k_fast,
                    k_primary,
                    i_u32,
                    keys,
                    state_hashes,
                )
                .is_lt()
                {
                    heap_fast[pos] = (p_fast, p_primary, p_idx);
                    pos = parent;
                } else {
                    break;
                }
            }
            heap_fast[pos] = (k_fast, k_primary, i_u32);
            if heap_fast.len() == total_k {
                (w_fast, w_primary, w_idx) = heap_fast[0];
            }
        }
        i += 1;
    }
    while i < n {
        let k_fast = fast_keys[i];
        if k_fast >= w_fast && marks[i] == 0 {
            maybe_push_fast(
                i,
                k_fast,
                keys,
                state_hashes,
                heap_fast,
                &mut w_fast,
                &mut w_primary,
                &mut w_idx,
            );
        }
        i += 1;
    }
    if heap_fast.is_empty() {
        return 0;
    }
    heap_fast.sort_unstable_by(|&(fa, pa, ia), &(fb, pb, ib)| {
        cmp_fast_entry(fa, pa, ia, fb, pb, ib, keys, state_hashes)
    });
    let added = take.min(heap_fast.len());
    let extra = pareto_extra.min(heap_fast.len().saturating_sub(added));
    let mut iter = heap_fast.iter().map(|&(_, _, idx)| idx as usize);
    for idx in iter.by_ref().take(added) {
        if marks[idx] == 0 {
            marks[idx] = 1;
            out.push((keys[idx].primary, keys[idx].node_id));
        }
    }
    let mut p_cnt = 0usize;
    for idx in iter.take(extra) {
        pareto_out[p_cnt] = idx;
        p_cnt += 1;
    }
    *pareto_count_out = p_cnt;
    added
}

#[inline(always)]
const fn compute_sig_table_size(cap: usize) -> usize {
    let target = (cap as u64).saturating_mul(4);
    let target = if target < 64 { 64 } else { target };
    let mut size = 64usize;
    while (size as u64) < target && size < (1 << 30) {
        size <<= 1;
    }
    size
}

#[inline(always)]
fn get_sig_count(sig: u64, keys: &[u64], counts: &[u8], mask: usize) -> u8 {
    let mut slot = (sig as usize) & mask;
    loop {
        let k = keys[slot];
        if k == 0 {
            return 0;
        }
        if k == sig {
            return counts[slot];
        }
        slot = (slot + 1) & mask;
    }
}

#[inline(always)]
fn try_consume_sig(
    sig: u64,
    keys: &mut [u64],
    counts: &mut [u8],
    mask: usize,
    max_clones: u8,
) -> bool {
    let mut slot = (sig as usize) & mask;
    loop {
        let k = keys[slot];
        if k == 0 {
            keys[slot] = sig;
            counts[slot] = 1;
            return true;
        }
        if k == sig {
            let c = counts[slot];
            if c >= max_clones {
                return false;
            }
            counts[slot] = c + 1;
            return true;
        }
        slot = (slot + 1) & mask;
    }
}

#[inline(always)]
fn compare_theft(a: &ExtendedPruneEntry, b: &ExtendedPruneEntry) -> std::cmp::Ordering {
    a.outstanding_stolen
        .cmp(&b.outstanding_stolen)
        .then_with(|| a.forfeits_gold.cmp(&b.forfeits_gold))
}

#[inline(always)]
fn cmp_entry_strategy(
    a: &ExtendedPruneEntry,
    b: &ExtendedPruneEntry,
    allow_death_save_rescue: bool,
    preserve_theft: bool,
) -> std::cmp::Ordering {
    let mut ord = if !allow_death_save_rescue {
        a.death_save_use_count.cmp(&b.death_save_use_count)
    } else {
        std::cmp::Ordering::Equal
    };
    if ord.is_ne() {
        return ord;
    }
    if preserve_theft {
        ord = compare_theft(a, b);
        if ord.is_ne() {
            return ord;
        }
    }
    b.strategy_goal_hp_credit
        .cmp(&a.strategy_goal_hp_credit)
        .then_with(|| b.strategy_goal_count.cmp(&a.strategy_goal_count))
        .then_with(|| a.relic_distance.cmp(&b.relic_distance))
        .then_with(|| {
            a.effective_net_hp_loss_centi
                .cmp(&b.effective_net_hp_loss_centi)
        })
        .then_with(|| b.primary.total_cmp(&a.primary))
}

#[inline(always)]
fn cmp_entry_survival(
    a: &ExtendedPruneEntry,
    b: &ExtendedPruneEntry,
    allow_death_save_rescue: bool,
    preserve_theft: bool,
    has_any_strategy: bool,
) -> std::cmp::Ordering {
    if !allow_death_save_rescue {
        let ord = a.death_save_use_count.cmp(&b.death_save_use_count);
        if ord.is_ne() {
            return ord;
        }
    }
    if preserve_theft {
        let ord = compare_theft(a, b);
        if ord.is_ne() {
            return ord;
        }
    }
    let mut a_loss = a.effective_net_hp_loss_centi;
    let mut b_loss = b.effective_net_hp_loss_centi;
    if has_any_strategy {
        a_loss -= (a.strategic_hp_credit as i32) * weights::HP_FIXED_SCALE;
        b_loss -= (b.strategic_hp_credit as i32) * weights::HP_FIXED_SCALE;
    }
    let ord = a_loss.cmp(&b_loss);
    if ord.is_ne() {
        return ord;
    }
    let a_buf = a.best_hp + a.effective_defense;
    let b_buf = b.best_hp + b.effective_defense;
    b_buf
        .cmp(&a_buf)
        .then_with(|| b.best_hp.cmp(&a.best_hp))
        .then_with(|| {
            if allow_death_save_rescue {
                a.death_save_use_count.cmp(&b.death_save_use_count)
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .then_with(|| a.enemy_hp.cmp(&b.enemy_hp))
        .then_with(|| b.primary.total_cmp(&a.primary))
}

#[inline(always)]
fn cmp_entry_tempo(
    a: &ExtendedPruneEntry,
    b: &ExtendedPruneEntry,
    allow_death_save_rescue: bool,
    preserve_theft: bool,
) -> std::cmp::Ordering {
    if !allow_death_save_rescue {
        let ord = a.death_save_use_count.cmp(&b.death_save_use_count);
        if ord.is_ne() {
            return ord;
        }
    }
    if preserve_theft {
        let ord = compare_theft(a, b);
        if ord.is_ne() {
            return ord;
        }
    }
    a.rem_enemies
        .cmp(&b.rem_enemies)
        .then_with(|| a.enemy_hp.cmp(&b.enemy_hp))
        .then_with(|| {
            a.effective_net_hp_loss_centi
                .cmp(&b.effective_net_hp_loss_centi)
        })
        .then_with(|| b.primary.total_cmp(&a.primary))
}

#[inline(always)]
fn cmp_entry_board_investment(
    a: &ExtendedPruneEntry,
    b: &ExtendedPruneEntry,
    allow_death_save_rescue: bool,
    preserve_theft: bool,
) -> std::cmp::Ordering {
    if !allow_death_save_rescue {
        let ord = a.death_save_use_count.cmp(&b.death_save_use_count);
        if ord.is_ne() {
            return ord;
        }
    }
    if preserve_theft {
        let ord = compare_theft(a, b);
        if ord.is_ne() {
            return ord;
        }
    }
    let a_invest = (a.board_investment as i64) * 16_000 + (a.setup_score as i64) * 18_000;
    let b_invest = (b.board_investment as i64) * 16_000 + (b.setup_score as i64) * 18_000;
    b_invest
        .cmp(&a_invest)
        .then_with(|| {
            a.effective_net_hp_loss_centi
                .cmp(&b.effective_net_hp_loss_centi)
        })
        .then_with(|| b.setup_score.cmp(&a.setup_score))
        .then_with(|| b.primary.total_cmp(&a.primary))
}

#[inline(always)]
fn cmp_entry_optionality(
    a: &ExtendedPruneEntry,
    b: &ExtendedPruneEntry,
    is_turn_boundary: bool,
    allow_death_save_rescue: bool,
    preserve_theft: bool,
) -> std::cmp::Ordering {
    if !allow_death_save_rescue {
        let ord = a.death_save_use_count.cmp(&b.death_save_use_count);
        if ord.is_ne() {
            return ord;
        }
    }
    if preserve_theft {
        let ord = compare_theft(a, b);
        if ord.is_ne() {
            return ord;
        }
    }
    if !is_turn_boundary {
        let a_combo =
            a.optionality_score + (a.playable_hand_count as i32) + (a.energy as i32).min(6);
        let b_combo =
            b.optionality_score + (b.playable_hand_count as i32) + (b.energy as i32).min(6);
        let ord = b_combo.cmp(&a_combo);
        if ord.is_ne() {
            return ord;
        }
    }
    let a_invest = (a.board_investment as i64) * 16_000 + (a.setup_score as i64) * 18_000;
    let b_invest = (b.board_investment as i64) * 16_000 + (b.setup_score as i64) * 18_000;
    b.optionality_score
        .cmp(&a.optionality_score)
        .then_with(|| b_invest.cmp(&a_invest))
        .then_with(|| {
            a.effective_net_hp_loss_centi
                .cmp(&b.effective_net_hp_loss_centi)
        })
        .then_with(|| b.primary.total_cmp(&a.primary))
}

#[inline(always)]
fn cmp_entry_pure_rollout(a: &ExtendedPruneEntry, b: &ExtendedPruneEntry) -> std::cmp::Ordering {
    b.has_rollout_score
        .cmp(&a.has_rollout_score)
        .then_with(|| {
            if a.has_rollout_score && b.has_rollout_score {
                b.aux_score.total_cmp(&a.aux_score)
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .then_with(|| b.primary.total_cmp(&a.primary))
}

#[inline(always)]
fn cmp_entry_choice_lookahead(
    a: &ExtendedPruneEntry,
    b: &ExtendedPruneEntry,
) -> std::cmp::Ordering {
    b.has_choice_lookahead
        .cmp(&a.has_choice_lookahead)
        .then_with(|| {
            if a.has_choice_lookahead && b.has_choice_lookahead {
                b.aux_score.total_cmp(&a.aux_score)
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .then_with(|| b.primary.total_cmp(&a.primary))
}

#[allow(clippy::too_many_arguments)]
#[inline]
fn dominates_candidate(
    other: &ExtendedPruneEntry,
    cand: &ExtendedPruneEntry,
    allow_death_save_rescue: bool,
    preserve_theft: bool,
    forced_potion_mask: u64,
    require_explicit_potion: bool,
    has_pure_growth_targets: bool,
    has_relic_targets: bool,
) -> bool {
    if allow_death_save_rescue {
        if other.death_save_use_count != cand.death_save_use_count {
            return false;
        }
    } else if other.death_save_use_count > cand.death_save_use_count {
        return false;
    }

    let mut strictly_better_special = false;

    if preserve_theft {
        if other.outstanding_stolen > cand.outstanding_stolen
            || (other.forfeits_gold && !cand.forfeits_gold)
        {
            return false;
        }
        if other.outstanding_stolen < cand.outstanding_stolen
            || (!other.forfeits_gold && cand.forfeits_gold)
        {
            strictly_better_special = true;
        }
    }

    if forced_potion_mask != 0 {
        let other_forced = other.forced_used_mask & forced_potion_mask;
        let cand_forced = cand.forced_used_mask & forced_potion_mask;
        if (other_forced & cand_forced) != cand_forced {
            return false;
        }
        if other_forced != cand_forced {
            strictly_better_special = true;
        }
    }

    if require_explicit_potion {
        if cand.explicit_potion_count > 0 && other.explicit_potion_count == 0 {
            return false;
        }
        if other.explicit_potion_count > 0 && cand.explicit_potion_count == 0 {
            strictly_better_special = true;
        }
    }

    if other.effective_net_hp_loss_centi > cand.effective_net_hp_loss_centi
        || other.best_hp < cand.best_hp
        || other.effective_defense < cand.effective_defense
        || other.enemy_hp > cand.enemy_hp
        || other.rem_enemies > cand.rem_enemies
        || other.setup_score < cand.setup_score
        || other.board_investment < cand.board_investment
        || other.max_dark_evoke < cand.max_dark_evoke
        || other.total_poison_and_doom < cand.total_poison_and_doom
        || other.optionality_score < cand.optionality_score
        || other.playable_hand_count < cand.playable_hand_count
        || other.optional_paid_potion_count > cand.optional_paid_potion_count
        || other.potion_cost > cand.potion_cost
    {
        return false;
    }

    if has_pure_growth_targets {
        if other.growth_hp_credit < cand.growth_hp_credit
            || other.growth_reward_count < cand.growth_reward_count
        {
            return false;
        }
        if other.growth_hp_credit > cand.growth_hp_credit
            || other.growth_reward_count > cand.growth_reward_count
        {
            strictly_better_special = true;
        }
    }

    if has_relic_targets {
        if (other.relic_satisfied_mask & cand.relic_satisfied_mask) != cand.relic_satisfied_mask {
            return false;
        }
        let other_relic_credit = other.relic_hp_credit + other.relic_healing_hp_credit;
        let cand_relic_credit = cand.relic_hp_credit + cand.relic_healing_hp_credit;
        if other_relic_credit < cand_relic_credit
            || other.relic_satisfied_priority < cand.relic_satisfied_priority
            || other.relic_distance > cand.relic_distance
        {
            return false;
        }
        if other.relic_satisfied_mask != cand.relic_satisfied_mask
            || other_relic_credit > cand_relic_credit
            || other.relic_satisfied_priority > cand.relic_satisfied_priority
            || other.relic_distance < cand.relic_distance
        {
            strictly_better_special = true;
        }
    }

    if other.primary < cand.primary {
        return false;
    }

    strictly_better_special
        || other.death_save_use_count < cand.death_save_use_count
        || other.effective_net_hp_loss_centi < cand.effective_net_hp_loss_centi
        || other.best_hp > cand.best_hp
        || other.effective_defense > cand.effective_defense
        || other.enemy_hp < cand.enemy_hp
        || other.rem_enemies < cand.rem_enemies
        || other.setup_score > cand.setup_score
        || other.board_investment > cand.board_investment
        || other.max_dark_evoke > cand.max_dark_evoke
        || other.total_poison_and_doom > cand.total_poison_and_doom
        || other.optionality_score > cand.optionality_score
        || other.playable_hand_count > cand.playable_hand_count
        || other.optional_paid_potion_count < cand.optional_paid_potion_count
        || other.potion_cost < cand.potion_cost
        || other.primary > cand.primary
}

#[allow(clippy::too_many_arguments)]
#[inline]
fn is_dominated_in_full_pool_pareto(
    cand: &ExtendedPruneEntry,
    entries: &[ExtendedPruneEntry],
    marks: &[u8],
    node_sigs: &[u64],
    sig_keys: &[u64],
    sig_counts: &[u8],
    sig_mask: usize,
    max_clones_per_sig: u8,
    selected_indices: &[usize],
    pareto_pool: &[usize],
    cand_pool_pos: usize,
    allow_death_save_rescue: bool,
    preserve_theft: bool,
    forced_potion_mask: u64,
    require_explicit_potion: bool,
    has_pure_growth_targets: bool,
    has_relic_targets: bool,
) -> bool {
    for &sel_idx in selected_indices {
        if dominates_candidate(
            &entries[sel_idx],
            cand,
            allow_death_save_rescue,
            preserve_theft,
            forced_potion_mask,
            require_explicit_potion,
            has_pure_growth_targets,
            has_relic_targets,
        ) {
            return true;
        }
    }
    for (j, &other_idx) in pareto_pool.iter().enumerate() {
        if j == cand_pool_pos || marks[other_idx] == 1 {
            continue;
        }
        if get_sig_count(node_sigs[other_idx], sig_keys, sig_counts, sig_mask)
            >= max_clones_per_sig
        {
            continue;
        }
        if dominates_candidate(
            &entries[other_idx],
            cand,
            allow_death_save_rescue,
            preserve_theft,
            forced_potion_mask,
            require_explicit_potion,
            has_pure_growth_targets,
            has_relic_targets,
        ) {
            return true;
        }
    }
    false
}

#[inline(always)]
fn is_better_entry<F>(
    idx_a: usize,
    idx_b: usize,
    entries: &[ExtendedPruneEntry],
    keys: &[PruneKey],
    state_hashes: &[crate::hash::StateHash],
    cmp_entry: &F,
) -> bool
where
    F: Fn(&ExtendedPruneEntry, &ExtendedPruneEntry) -> std::cmp::Ordering,
{
    let ord = cmp_entry(&entries[idx_a], &entries[idx_b]);
    if ord.is_lt() {
        true
    } else if ord.is_gt() {
        false
    } else {
        cmp_key_tie_break(keys, state_hashes, idx_a, idx_b).is_lt()
    }
}

#[inline(always)]
fn sift_down_niche_heap<F>(
    heap: &mut [usize],
    start_pos: usize,
    entries: &[ExtendedPruneEntry],
    keys: &[PruneKey],
    state_hashes: &[crate::hash::StateHash],
    cmp_entry: &F,
) where
    F: Fn(&ExtendedPruneEntry, &ExtendedPruneEntry) -> std::cmp::Ordering,
{
    let len = heap.len();
    let mut pos = start_pos;
    loop {
        let left = (pos << 1) + 1;
        if left >= len {
            break;
        }
        let mut worst = pos;
        if is_better_entry(heap[worst], heap[left], entries, keys, state_hashes, cmp_entry) {
            worst = left;
        }
        let right = left + 1;
        if right < len
            && is_better_entry(
                heap[worst],
                heap[right],
                entries,
                keys,
                state_hashes,
                cmp_entry,
            )
        {
            worst = right;
        }
        if worst == pos {
            break;
        }
        heap.swap(pos, worst);
        pos = worst;
    }
}

#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn heap_sig_increment<F>(
    sig: u64,
    cand_idx: usize,
    heap_keys: &mut [u64],
    heap_counts: &mut [u8],
    heap_worst_idx: &mut [i32],
    mask: usize,
    entries: &[ExtendedPruneEntry],
    keys: &[PruneKey],
    state_hashes: &[crate::hash::StateHash],
    cmp_entry: &F,
) where
    F: Fn(&ExtendedPruneEntry, &ExtendedPruneEntry) -> std::cmp::Ordering,
{
    let mut slot = (sig as usize) & mask;
    loop {
        let k = heap_keys[slot];
        if k == 0 {
            heap_keys[slot] = sig;
            heap_counts[slot] = 1;
            heap_worst_idx[slot] = cand_idx as i32;
            return;
        }
        if k == sig {
            heap_counts[slot] = heap_counts[slot].saturating_add(1);
            let prev_worst = heap_worst_idx[slot];
            if prev_worst >= 0
                && is_better_entry(
                    prev_worst as usize,
                    cand_idx,
                    entries,
                    keys,
                    state_hashes,
                    cmp_entry,
                )
            {
                heap_worst_idx[slot] = cand_idx as i32;
            }
            return;
        }
        slot = (slot + 1) & mask;
    }
}

#[inline]
fn heap_sig_decrement(
    sig: u64,
    heap_keys: &mut [u64],
    heap_counts: &mut [u8],
    heap_worst_idx: &mut [i32],
    mask: usize,
) {
    let mut slot = (sig as usize) & mask;
    loop {
        let k = heap_keys[slot];
        if k == 0 {
            return;
        }
        if k == sig {
            break;
        }
        slot = (slot + 1) & mask;
    }
    let c = heap_counts[slot];
    if c > 1 {
        heap_counts[slot] = c - 1;
        heap_worst_idx[slot] = -1;
        return;
    }
    let mut hole = slot;
    let mut cur = (hole + 1) & mask;
    loop {
        let k = heap_keys[cur];
        if k == 0 {
            heap_keys[hole] = 0;
            heap_counts[hole] = 0;
            heap_worst_idx[hole] = -1;
            return;
        }
        let home = (k as usize) & mask;
        if ((hole.wrapping_sub(home)) & mask) < ((cur.wrapping_sub(home)) & mask) {
            heap_keys[hole] = k;
            heap_counts[hole] = heap_counts[cur];
            heap_worst_idx[hole] = heap_worst_idx[cur];
            hole = cur;
        }
        cur = (cur + 1) & mask;
    }
}

#[allow(clippy::too_many_arguments)]
fn select_top_k_unmarked<F>(
    entries: &[ExtendedPruneEntry],
    keys: &[PruneKey],
    state_hashes: &[crate::hash::StateHash],
    node_sigs: &[u64],
    marks: &mut [u8],
    sig_keys: &mut [u64],
    sig_counts: &mut [u8],
    sig_mask: usize,
    max_clones_per_sig: u8,
    heap: &mut Vec<usize>,
    heap_sig_keys: &mut Vec<u64>,
    heap_sig_counts: &mut Vec<u8>,
    heap_sig_worst_idx: &mut Vec<i32>,
    selected_indices: &mut Vec<usize>,
    take: usize,
    pareto_extra: usize,
    out: &mut Vec<(f64, NodeId)>,
    pareto_out: &mut [usize; PER_NICHE_PARETO_TOP_K],
    pareto_count_out: &mut usize,
    cmp_entry: F,
) -> usize
where
    F: Fn(&ExtendedPruneEntry, &ExtendedPruneEntry) -> std::cmp::Ordering,
{
    *pareto_count_out = 0;
    let total_k = take + pareto_extra;
    if total_k == 0 {
        return 0;
    }

    let heap_sig_table_size = compute_sig_table_size(total_k);
    let heap_sig_mask = heap_sig_table_size - 1;
    heap_sig_keys.clear();
    heap_sig_keys.resize(heap_sig_table_size, 0);
    heap_sig_counts.clear();
    heap_sig_counts.resize(heap_sig_table_size, 0);
    heap_sig_worst_idx.clear();
    heap_sig_worst_idx.resize(heap_sig_table_size, -1);

    heap.clear();
    for i in 0..entries.len() {
        if marks[i] != 0 {
            continue;
        }
        if heap.len() == total_k
            && !is_better_entry(i, heap[0], entries, keys, state_hashes, &cmp_entry)
        {
            continue;
        }

        let sig = node_sigs[i];
        let rem = (max_clones_per_sig as i32)
            - (get_sig_count(sig, sig_keys, sig_counts, sig_mask) as i32);
        if rem <= 0 {
            continue;
        }

        let mut slot = (sig as usize) & heap_sig_mask;
        let mut in_heap = 0i32;
        loop {
            let k = heap_sig_keys[slot];
            if k == 0 {
                break;
            }
            if k == sig {
                in_heap = heap_sig_counts[slot] as i32;
                break;
            }
            slot = (slot + 1) & heap_sig_mask;
        }

        if in_heap < rem {
            if heap.len() < total_k {
                let mut pos = heap.len();
                heap.push(i);
                while pos > 0 {
                    let parent = (pos - 1) >> 1;
                    if is_better_entry(
                        heap[parent],
                        heap[pos],
                        entries,
                        keys,
                        state_hashes,
                        &cmp_entry,
                    ) {
                        heap.swap(parent, pos);
                        pos = parent;
                    } else {
                        break;
                    }
                }
                if in_heap == 0 {
                    heap_sig_keys[slot] = sig;
                    heap_sig_counts[slot] = 1;
                    heap_sig_worst_idx[slot] = i as i32;
                } else {
                    heap_sig_counts[slot] = (in_heap + 1) as u8;
                    let prev_worst = heap_sig_worst_idx[slot];
                    if prev_worst >= 0
                        && is_better_entry(
                            prev_worst as usize,
                            i,
                            entries,
                            keys,
                            state_hashes,
                            &cmp_entry,
                        )
                    {
                        heap_sig_worst_idx[slot] = i as i32;
                    }
                }
            } else {
                let evicted = heap[0];
                let evicted_sig = node_sigs[evicted];
                heap[0] = i;
                sift_down_niche_heap(heap, 0, entries, keys, state_hashes, &cmp_entry);
                if evicted_sig == sig {
                    heap_sig_worst_idx[slot] = if in_heap == 1 { i as i32 } else { -1 };
                } else {
                    heap_sig_decrement(
                        evicted_sig,
                        heap_sig_keys,
                        heap_sig_counts,
                        heap_sig_worst_idx,
                        heap_sig_mask,
                    );
                    heap_sig_increment(
                        sig,
                        i,
                        heap_sig_keys,
                        heap_sig_counts,
                        heap_sig_worst_idx,
                        heap_sig_mask,
                        entries,
                        keys,
                        state_hashes,
                        &cmp_entry,
                    );
                }
            }
        } else {
            let cached_worst = heap_sig_worst_idx[slot];
            if cached_worst >= 0
                && !is_better_entry(
                    i,
                    cached_worst as usize,
                    entries,
                    keys,
                    state_hashes,
                    &cmp_entry,
                )
            {
                continue;
            }
            let mut worst_pos = None;
            for (h, &cand_h) in heap.iter().enumerate() {
                if node_sigs[cand_h] != sig {
                    continue;
                }
                if worst_pos.is_none_or(|wp| {
                    is_better_entry(heap[wp], cand_h, entries, keys, state_hashes, &cmp_entry)
                }) {
                    worst_pos = Some(h);
                }
            }
            let Some(wp) = worst_pos else {
                continue;
            };
            if !is_better_entry(i, heap[wp], entries, keys, state_hashes, &cmp_entry) {
                heap_sig_worst_idx[slot] = heap[wp] as i32;
                continue;
            }
            heap[wp] = i;
            sift_down_niche_heap(heap, wp, entries, keys, state_hashes, &cmp_entry);
            heap_sig_worst_idx[slot] = if rem == 1 { i as i32 } else { -1 };
        }
    }

    if heap.is_empty() {
        return 0;
    }

    heap.sort_unstable_by(|&a, &b| {
        cmp_entry(&entries[a], &entries[b])
            .then_with(|| cmp_key_tie_break(keys, state_hashes, a, b))
    });

    let added = take.min(heap.len());
    for &idx in heap.iter().take(added) {
        marks[idx] = 1;
        out.push((entries[idx].primary, entries[idx].node_id));
        selected_indices.push(idx);
        try_consume_sig(
            node_sigs[idx],
            sig_keys,
            sig_counts,
            sig_mask,
            max_clones_per_sig,
        );
    }

    let extra = pareto_extra
        .min(heap.len().saturating_sub(added))
        .min(PER_NICHE_PARETO_TOP_K);
    for (p, &idx) in heap.iter().skip(added).take(extra).enumerate() {
        pareto_out[p] = idx;
    }
    *pareto_count_out = extra;
    added
}

#[allow(clippy::too_many_arguments)]
fn select_from_potion_bucket_scratch<S: PruneSource>(
    source: &S,
    bucket: &mut BucketPruneScratch,
    quota: usize,
    bucket_index: usize,
    root: &RootEvalBaseline,
    second_rank_band: bool,
    aggressive_power_commitment: bool,
    prefer_potion_buckets: bool,
    is_turn_boundary: bool,
    use_rollout_scores: bool,
    allow_death_save_rescue: bool,
    pool: Option<&rayon::ThreadPool>,
) {
    let BucketPruneScratch {
        nodes: bucket_nodes,
        out,
        marks,
        unmarked,
        heap,
        keys,
        entries,
        state_hashes,
        node_sigs,
        sig_keys,
        sig_counts,
        heap_sig_keys,
        heap_sig_counts,
        heap_sig_worst_idx,
        selected_indices,
        order,
    } = bucket;
    out.clear();
    let count = bucket_nodes.len();
    if count <= quota {
        out.reserve(count);
        for &nid in bucket_nodes.iter() {
            out.push((source.beam_score(nid, use_rollout_scores), nid));
        }
        return;
    }

    let has_pure_growth = !root.ignore_long_term_rewards && root.effective_has_growth_targets;
    let has_relic = !root.active_relic_targets.is_empty();
    let has_any_strategy = has_pure_growth || has_relic;
    let preserve_theft = root.theft_policy == SolverTheftPolicy::PreserveResources;
    let forced_potion_mask = root.all_forced_potion_mask;
    let require_explicit_potion = root.initial_player_max_hp > 0
        && root.effective_potion_policy == SolverPotionPolicy::RequireAtLeastOne;

    keys.clear();
    entries.clear();
    state_hashes.clear();
    node_sigs.clear();
    order.clear();
    keys.reserve(count);
    entries.reserve(count);
    state_hashes.reserve(count);
    node_sigs.reserve(count);
    order.reserve(count);

    if let Some(pool) = pool.filter(|_| count >= 1024) {
        let k_uninit = &mut keys.spare_capacity_mut()[..count];
        let e_uninit = &mut entries.spare_capacity_mut()[..count];
        let sh_uninit = &mut state_hashes.spare_capacity_mut()[..count];
        let sig_uninit = &mut node_sigs.spare_capacity_mut()[..count];
        let ord_uninit = &mut order.spare_capacity_mut()[..count];
        let chunk_len = ((count + 15) / 16).max(64);
        let mut chunk_iter = k_uninit
            .chunks_mut(chunk_len)
            .zip(e_uninit.chunks_mut(chunk_len))
            .zip(sh_uninit.chunks_mut(chunk_len))
            .zip(sig_uninit.chunks_mut(chunk_len))
            .zip(ord_uninit.chunks_mut(chunk_len))
            .zip(bucket_nodes.chunks(chunk_len));
        let chunk0 = chunk_iter.next();
        pool.scope(|scope| {
            let mut offset = chunk0.as_ref().map_or(0, |(_, n)| n.len());
            for (((((k_chunk, e_chunk), sh_chunk), sig_chunk), ord_chunk), n_chunk) in chunk_iter {
                let start = offset;
                offset += k_chunk.len();
                scope.spawn(move |_| {
                    for j in 0..k_chunk.len() {
                        let nid = n_chunk[j];
                        let (k, _, _, _, _, _, sh) =
                            source.build_prune_entry(nid, root, use_rollout_scores);
                        let ext =
                            source.build_extended_prune_entry(nid, &k, root, use_rollout_scores);
                        let ord_val = (k.primary, (start + j) as u32);
                        k_chunk[j].write(k);
                        e_chunk[j].write(ext);
                        sh_chunk[j].write(sh);
                        sig_chunk[j].write(ext.tactical_sig);
                        ord_chunk[j].write(ord_val);
                    }
                });
            }
            if let Some((((((k_chunk, e_chunk), sh_chunk), sig_chunk), ord_chunk), n_chunk)) =
                chunk0
            {
                for j in 0..k_chunk.len() {
                    let nid = n_chunk[j];
                    let (k, _, _, _, _, _, sh) =
                        source.build_prune_entry(nid, root, use_rollout_scores);
                    let ext =
                        source.build_extended_prune_entry(nid, &k, root, use_rollout_scores);
                    let ord_val = (k.primary, j as u32);
                    k_chunk[j].write(k);
                    e_chunk[j].write(ext);
                    sh_chunk[j].write(sh);
                    sig_chunk[j].write(ext.tactical_sig);
                    ord_chunk[j].write(ord_val);
                }
            }
        });
        // SAFETY: Disjoint chunks initialized all `0..count` elements inside `pool.scope`.
        unsafe {
            keys.set_len(count);
            entries.set_len(count);
            state_hashes.set_len(count);
            node_sigs.set_len(count);
            order.set_len(count);
        }
    } else {
        for (i, &nid) in bucket_nodes.iter().enumerate() {
            let (k, _, _, _, _, _, sh) = source.build_prune_entry(nid, root, use_rollout_scores);
            let ext = source.build_extended_prune_entry(nid, &k, root, use_rollout_scores);
            order.push((k.primary, i as u32));
            keys.push(k);
            entries.push(ext);
            state_hashes.push(sh);
            node_sigs.push(ext.tactical_sig);
        }
    }

    // Full canonical ranking so signature clone capping in Pass 1 + Pass 2 spillover stays exact.
    order.sort_unstable_by(|&(pa, ia), &(pb, ib)| {
        pb.total_cmp(&pa)
            .then_with(|| cmp_key_tie_break(keys, state_hashes, ia as usize, ib as usize))
    });

    let sig_table_size = compute_sig_table_size(quota);
    let sig_mask = sig_table_size - 1;
    sig_keys.clear();
    sig_keys.resize(sig_table_size, 0);
    sig_counts.clear();
    sig_counts.resize(sig_table_size, 0);
    let max_clones_per_sig: u8 = if is_turn_boundary { 2 } else { 3 };

    // 0 = unselected, 1 = selected into `out`, 2 = queued in full-pool Pareto candidate set
    marks.clear();
    marks.resize(count, 0);
    selected_indices.clear();
    let mut selected_in_bucket = 0usize;

    // 0. Non-starving potion-signature diversity inside potion buckets (b >= 1):
    if bucket_index >= 1 && quota >= 6 {
        let pct_cap = if prefer_potion_buckets {
            (quota * 25) / 100
        } else {
            (quota * 15) / 100
        };
        let potion_diversity_quota = MAX_DISTINCT_POTION_SIGNATURES
            .min(pct_cap.min(quota.saturating_sub(5)).max(1))
            .min(quota - selected_in_bucket);

        let mut seen_keys = [0u32; MAX_DISTINCT_POTION_SIGNATURES];
        let mut seen_count = 0usize;

        for &(_, idx_u32) in order.iter() {
            if selected_in_bucket >= potion_diversity_quota
                || seen_count >= MAX_DISTINCT_POTION_SIGNATURES
            {
                break;
            }
            let i = idx_u32 as usize;
            let e = &entries[i];
            let ps_sig =
                ((e.used_potion_slot_mask as u32) << 16) | ((e.potion_cost as u16) as u32);
            if seen_keys[..seen_count].contains(&ps_sig) {
                continue;
            }
            if !try_consume_sig(
                node_sigs[i],
                sig_keys,
                sig_counts,
                sig_mask,
                max_clones_per_sig,
            ) {
                continue;
            }
            seen_keys[seen_count] = ps_sig;
            seen_count += 1;
            marks[i] = 1;
            out.push((e.primary, e.node_id));
            selected_indices.push(i);
            selected_in_bucket += 1;
        }

        if quota >= 16 && seen_count > 1 && selected_in_bucket < potion_diversity_quota {
            for &target_sig in &seen_keys[..seen_count] {
                if selected_in_bucket >= potion_diversity_quota || selected_in_bucket >= quota {
                    break;
                }
                for &(_, idx_u32) in order.iter() {
                    let i = idx_u32 as usize;
                    if marks[i] != 0 {
                        continue;
                    }
                    let e = &entries[i];
                    let ps_sig =
                        ((e.used_potion_slot_mask as u32) << 16) | ((e.potion_cost as u16) as u32);
                    if ps_sig == target_sig
                        && try_consume_sig(
                            node_sigs[i],
                            sig_keys,
                            sig_counts,
                            sig_mask,
                            max_clones_per_sig,
                        )
                    {
                        marks[i] = 1;
                        out.push((e.primary, e.node_id));
                        selected_indices.push(i);
                        selected_in_bucket += 1;
                        break;
                    }
                }
            }
        }
    }

    let rem = quota.saturating_sub(selected_in_bucket);
    if rem == 0 {
        return;
    }

    // 1. 5 MAP-Elites niche quotas + Niche 5b true full-pool Pareto
    let (
        mut primary_q,
        mut survival_q,
        mut tempo_q,
        mut setup_q,
        mut optionality_q,
        mut pareto_q,
    ) = compute_niche_quotas(rem, is_turn_boundary, aggressive_power_commitment);

    // Carved Strategy Niche: only when `has_any_strategy` and `rem >= 3`.
    let mut strategy_q = 0usize;
    if has_any_strategy && rem >= 3 {
        let desired_strategy = if rem < 6 {
            1
        } else {
            (rem / 10).clamp(2, 12)
        };
        let avail_primary = primary_q.saturating_sub(1);
        let avail_opt = optionality_q;
        let pool_avail = avail_primary + avail_opt;
        if desired_strategy <= pool_avail {
            let mut from_opt = if pool_avail > 0 {
                avail_opt.min((desired_strategy * avail_opt + (pool_avail >> 1)) / pool_avail)
            } else {
                0
            };
            let mut from_primary = desired_strategy - from_opt;
            if from_primary > avail_primary {
                let excess = from_primary - avail_primary;
                from_primary = avail_primary;
                from_opt = avail_opt.min(from_opt + excess);
            }
            primary_q -= from_primary;
            optionality_q -= from_opt;
            strategy_q = from_primary + from_opt;
        } else {
            primary_q -= avail_primary;
            optionality_q -= avail_opt;
            strategy_q = avail_primary + avail_opt;
            let mut still_need = desired_strategy - strategy_q;
            if still_need > 0 && pareto_q > 0 {
                let d = still_need.min(pareto_q);
                pareto_q -= d;
                strategy_q += d;
                still_need -= d;
            }
            if still_need > 0 && tempo_q > 0 {
                let d = still_need.min(tempo_q);
                tempo_q -= d;
                strategy_q += d;
                still_need -= d;
            }
            if still_need > 0 && setup_q > 0 {
                let d = still_need.min(setup_q);
                setup_q -= d;
                strategy_q += d;
                still_need -= d;
            }
            if still_need > 0 && survival_q > 1 {
                let d = still_need.min(survival_q - 1);
                survival_q -= d;
                strategy_q += d;
            }
        }
    }

    let mut niche_pareto = [[0usize; PER_NICHE_PARETO_TOP_K]; NICHE_SLOT_COUNT];
    let mut niche_pareto_counts = [0usize; NICHE_SLOT_COUNT];

    let has_rollout = use_rollout_scores && entries.iter().any(|e| e.has_rollout_score);
    let has_choice_lookahead =
        !is_turn_boundary && !has_rollout && entries.iter().any(|e| e.has_choice_lookahead);

    // Niche 1 — Primary Value (>=34% non-strategy budget, sidecar PureRollout 20% / ChoiceLookahead 18%)
    if primary_q > 0 || pareto_q > 0 {
        let non_strategy_budget = rem.saturating_sub(strategy_q).max(1);
        let min_blended_primary = primary_q.min(((non_strategy_budget * 34 + 99) / 100).max(1));
        let mut max_primary_sidecar = primary_q.saturating_sub(min_blended_primary);
        let sidecar_floor = if primary_q >= 6 {
            2
        } else if primary_q >= 3 {
            1
        } else {
            0
        };
        if (has_rollout || has_choice_lookahead) && primary_q >= 6 && max_primary_sidecar < 2 {
            max_primary_sidecar = 2usize.min(primary_q.saturating_sub(1));
        }
        let rollout_pure_q = if has_rollout && max_primary_sidecar > 0 && primary_q >= 3 {
            sidecar_floor
                .max((primary_q * 20) / 100)
                .min(max_primary_sidecar)
        } else {
            0
        };
        let choice_lookahead_q = if has_choice_lookahead && max_primary_sidecar > 0 && primary_q >= 3
        {
            sidecar_floor
                .max((primary_q * 18) / 100)
                .min(max_primary_sidecar)
        } else {
            0
        };
        let blended_q = primary_q.saturating_sub(rollout_pure_q + choice_lookahead_q);
        let take_blended = blended_q.min(quota - selected_in_bucket);
        let mut taken_blended = 0usize;
        let mut p_cnt = 0usize;

        if second_rank_band && blended_q > 0 {
            unmarked.clear();
            unmarked.extend(
                order
                    .iter()
                    .map(|&(_, i_u32)| i_u32 as usize)
                    .filter(|&i| marks[i] == 0),
            );
            BeamWidthPortfolio::move_leading_band_to_tail(unmarked, blended_q);
            for &idx in unmarked.iter() {
                let sig = node_sigs[idx];
                if taken_blended < take_blended && selected_in_bucket < quota {
                    if !try_consume_sig(sig, sig_keys, sig_counts, sig_mask, max_clones_per_sig) {
                        continue;
                    }
                    marks[idx] = 1;
                    out.push((entries[idx].primary, entries[idx].node_id));
                    selected_indices.push(idx);
                    selected_in_bucket += 1;
                    taken_blended += 1;
                } else if pareto_q > 0 && selected_in_bucket < quota && p_cnt < PER_NICHE_PARETO_TOP_K
                {
                    let existing_count = get_sig_count(sig, sig_keys, sig_counts, sig_mask);
                    if existing_count >= max_clones_per_sig {
                        continue;
                    }
                    let queued_same_sig = niche_pareto[0][..p_cnt]
                        .iter()
                        .filter(|&&q| node_sigs[q] == sig)
                        .count();
                    if existing_count as usize + queued_same_sig >= max_clones_per_sig as usize {
                        continue;
                    }
                    niche_pareto[0][p_cnt] = idx;
                    p_cnt += 1;
                } else if taken_blended >= take_blended || selected_in_bucket >= quota {
                    break;
                }
            }
        } else {
            for &(_, idx_u32) in order.iter() {
                let idx = idx_u32 as usize;
                if marks[idx] != 0 {
                    continue;
                }
                let sig = node_sigs[idx];
                if taken_blended < take_blended && selected_in_bucket < quota {
                    if !try_consume_sig(sig, sig_keys, sig_counts, sig_mask, max_clones_per_sig) {
                        continue;
                    }
                    marks[idx] = 1;
                    out.push((entries[idx].primary, entries[idx].node_id));
                    selected_indices.push(idx);
                    selected_in_bucket += 1;
                    taken_blended += 1;
                } else if pareto_q > 0 && selected_in_bucket < quota && p_cnt < PER_NICHE_PARETO_TOP_K
                {
                    let existing_count = get_sig_count(sig, sig_keys, sig_counts, sig_mask);
                    if existing_count >= max_clones_per_sig {
                        continue;
                    }
                    let queued_same_sig = niche_pareto[0][..p_cnt]
                        .iter()
                        .filter(|&&q| node_sigs[q] == sig)
                        .count();
                    if existing_count as usize + queued_same_sig >= max_clones_per_sig as usize {
                        continue;
                    }
                    niche_pareto[0][p_cnt] = idx;
                    p_cnt += 1;
                } else if taken_blended >= take_blended || selected_in_bucket >= quota {
                    break;
                }
            }
        }
        niche_pareto_counts[0] = p_cnt;

        if has_rollout && (rollout_pure_q > 0 || pareto_q > 0) && selected_in_bucket < quota {
            let take_roll = rollout_pure_q.min(quota - selected_in_bucket);
            selected_in_bucket += select_top_k_unmarked(
                entries,
                keys,
                state_hashes,
                node_sigs,
                marks,
                sig_keys,
                sig_counts,
                sig_mask,
                max_clones_per_sig,
                heap,
                heap_sig_keys,
                heap_sig_counts,
                heap_sig_worst_idx,
                selected_indices,
                take_roll,
                if pareto_q > 0 { PER_NICHE_PARETO_TOP_K } else { 0 },
                out,
                &mut niche_pareto[5],
                &mut niche_pareto_counts[5],
                cmp_entry_pure_rollout,
            );
        } else if has_choice_lookahead
            && (choice_lookahead_q > 0 || pareto_q > 0)
            && selected_in_bucket < quota
        {
            let take_la = choice_lookahead_q.min(quota - selected_in_bucket);
            selected_in_bucket += select_top_k_unmarked(
                entries,
                keys,
                state_hashes,
                node_sigs,
                marks,
                sig_keys,
                sig_counts,
                sig_mask,
                max_clones_per_sig,
                heap,
                heap_sig_keys,
                heap_sig_counts,
                heap_sig_worst_idx,
                selected_indices,
                take_la,
                if pareto_q > 0 { PER_NICHE_PARETO_TOP_K } else { 0 },
                out,
                &mut niche_pareto[5],
                &mut niche_pareto_counts[5],
                cmp_entry_choice_lookahead,
            );
        }
    }

    let pareto_extra = if pareto_q > 0 {
        PER_NICHE_PARETO_TOP_K
    } else {
        0
    };

    // Carved Strategy Niche
    if has_any_strategy && (strategy_q > 0 || pareto_q > 0) && selected_in_bucket < quota {
        let take_strategy = strategy_q.min(quota - selected_in_bucket);
        selected_in_bucket += select_top_k_unmarked(
            entries,
            keys,
            state_hashes,
            node_sigs,
            marks,
            sig_keys,
            sig_counts,
            sig_mask,
            max_clones_per_sig,
            heap,
            heap_sig_keys,
            heap_sig_counts,
            heap_sig_worst_idx,
            selected_indices,
            take_strategy,
            pareto_extra,
            out,
            &mut niche_pareto[6],
            &mut niche_pareto_counts[6],
            |a, b| cmp_entry_strategy(a, b, allow_death_save_rescue, preserve_theft),
        );
    }

    // Niche 2 — Survival / HP Preservation
    if (survival_q > 0 || pareto_q > 0) && selected_in_bucket < quota {
        let take_survival = survival_q.min(quota - selected_in_bucket);
        selected_in_bucket += select_top_k_unmarked(
            entries,
            keys,
            state_hashes,
            node_sigs,
            marks,
            sig_keys,
            sig_counts,
            sig_mask,
            max_clones_per_sig,
            heap,
            heap_sig_keys,
            heap_sig_counts,
            heap_sig_worst_idx,
            selected_indices,
            take_survival,
            pareto_extra,
            out,
            &mut niche_pareto[1],
            &mut niche_pareto_counts[1],
            |a, b| cmp_entry_survival(a, b, allow_death_save_rescue, preserve_theft, has_any_strategy),
        );
    }

    // Niche 3 — Tempo / Kill Speed
    if (tempo_q > 0 || pareto_q > 0) && selected_in_bucket < quota {
        let take_tempo = tempo_q.min(quota - selected_in_bucket);
        selected_in_bucket += select_top_k_unmarked(
            entries,
            keys,
            state_hashes,
            node_sigs,
            marks,
            sig_keys,
            sig_counts,
            sig_mask,
            max_clones_per_sig,
            heap,
            heap_sig_keys,
            heap_sig_counts,
            heap_sig_worst_idx,
            selected_indices,
            take_tempo,
            pareto_extra,
            out,
            &mut niche_pareto[2],
            &mut niche_pareto_counts[2],
            |a, b| cmp_entry_tempo(a, b, allow_death_save_rescue, preserve_theft),
        );
    }

    // Niche 4 — Generic Persistent Board Investment
    if (setup_q > 0 || pareto_q > 0) && selected_in_bucket < quota {
        let take_setup = setup_q.min(quota - selected_in_bucket);
        selected_in_bucket += select_top_k_unmarked(
            entries,
            keys,
            state_hashes,
            node_sigs,
            marks,
            sig_keys,
            sig_counts,
            sig_mask,
            max_clones_per_sig,
            heap,
            heap_sig_keys,
            heap_sig_counts,
            heap_sig_worst_idx,
            selected_indices,
            take_setup,
            pareto_extra,
            out,
            &mut niche_pareto[3],
            &mut niche_pareto_counts[3],
            |a, b| cmp_entry_board_investment(a, b, allow_death_save_rescue, preserve_theft),
        );
    }

    // Niche 5a — Optionality / Mid-Turn Combo
    if (optionality_q > 0 || pareto_q > 0) && selected_in_bucket < quota {
        let take_opt = optionality_q.min(quota - selected_in_bucket);
        selected_in_bucket += select_top_k_unmarked(
            entries,
            keys,
            state_hashes,
            node_sigs,
            marks,
            sig_keys,
            sig_counts,
            sig_mask,
            max_clones_per_sig,
            heap,
            heap_sig_keys,
            heap_sig_counts,
            heap_sig_worst_idx,
            selected_indices,
            take_opt,
            pareto_extra,
            out,
            &mut niche_pareto[4],
            &mut niche_pareto_counts[4],
            |a, b| cmp_entry_optionality(a, b, is_turn_boundary, allow_death_save_rescue, preserve_theft),
        );
    }

    // Niche 5b — True Full-Pool Pareto scanning top-32 runners-up from EACH niche (14+ dim)
    if pareto_q > 0 && selected_in_bucket < quota {
        const NICHE_ORDER_WITH_STRATEGY: [usize; 7] = [6, 3, 1, 2, 4, 5, 0];
        const NICHE_ORDER_DEFAULT: [usize; 6] = [3, 1, 2, 4, 5, 0];
        let niche_order: &[usize] = if has_any_strategy {
            &NICHE_ORDER_WITH_STRATEGY
        } else {
            &NICHE_ORDER_DEFAULT
        };
        let mut pareto_pool = [0usize; NICHE_SLOT_COUNT * PER_NICHE_PARETO_TOP_K];
        let mut pareto_pool_count = 0usize;

        for rank in 0..PER_NICHE_PARETO_TOP_K {
            for &slot in niche_order {
                if rank >= niche_pareto_counts[slot] {
                    continue;
                }
                let idx = niche_pareto[slot][rank];
                if marks[idx] != 0 {
                    continue;
                }
                if get_sig_count(node_sigs[idx], sig_keys, sig_counts, sig_mask)
                    >= max_clones_per_sig
                {
                    continue;
                }
                marks[idx] = 2;
                pareto_pool[pareto_pool_count] = idx;
                pareto_pool_count += 1;
            }
        }

        let pareto_budget = pareto_q.min(quota - selected_in_bucket);
        let mut pareto_added = 0usize;
        for p in 0..pareto_pool_count {
            if pareto_added >= pareto_budget || selected_in_bucket >= quota {
                break;
            }
            let cand_idx = pareto_pool[p];
            let cand_sig = node_sigs[cand_idx];
            if get_sig_count(cand_sig, sig_keys, sig_counts, sig_mask) >= max_clones_per_sig {
                continue;
            }
            if is_dominated_in_full_pool_pareto(
                &entries[cand_idx],
                entries,
                marks,
                node_sigs,
                sig_keys,
                sig_counts,
                sig_mask,
                max_clones_per_sig,
                selected_indices,
                &pareto_pool[..pareto_pool_count],
                p,
                allow_death_save_rescue,
                preserve_theft,
                forced_potion_mask,
                require_explicit_potion,
                has_pure_growth,
                has_relic,
            ) {
                continue;
            }
            if !try_consume_sig(cand_sig, sig_keys, sig_counts, sig_mask, max_clones_per_sig) {
                continue;
            }
            marks[cand_idx] = 1;
            out.push((entries[cand_idx].primary, entries[cand_idx].node_id));
            selected_indices.push(cand_idx);
            selected_in_bucket += 1;
            pareto_added += 1;
        }
    }

    // Pass 1 spillover: fill unused quota in Primary order while still respecting signature cap.
    for &(_, i_u32) in order.iter() {
        let i = i_u32 as usize;
        if selected_in_bucket >= quota {
            break;
        }
        if marks[i] == 1 {
            continue;
        }
        if !try_consume_sig(
            node_sigs[i],
            sig_keys,
            sig_counts,
            sig_mask,
            max_clones_per_sig,
        ) {
            continue;
        }
        marks[i] = 1;
        out.push((entries[i].primary, entries[i].node_id));
        selected_indices.push(i);
        selected_in_bucket += 1;
    }

    // Pass 2 spillover (narrow-tree fallback): ignore signature cap so zero beam slots are wasted.
    for &(_, i_u32) in order.iter() {
        let i = i_u32 as usize;
        if selected_in_bucket >= quota {
            break;
        }
        if marks[i] == 1 {
            continue;
        }
        marks[i] = 1;
        out.push((entries[i].primary, entries[i].node_id));
        selected_in_bucket += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use crate::arena::{PackedHandle, SearchNode};
    use crate::eval::{StoredFastEval, StoredNodeEval};
    use crate::hash::StateHash;

    fn make_tied_node(
        arena: &mut NodeArena,
        hash_lo: u64,
        has_choice_lookahead: bool,
        rollout_score: Option<f64>,
    ) -> NodeId {
        let mut full = StoredNodeEval::default();
        full.beam_rank_score = 1234.0;
        full.has_choice_lookahead = has_choice_lookahead;
        full.choice_lookahead_score = if has_choice_lookahead {
            rollout_score
        } else {
            None
        };
        full.rollout_score = None;

        let mut fast = StoredFastEval::default();
        fast.cumulative_hp_lost = 5;
        fast.enemy_hp = 100;
        fast.alive_enemy_count = 1;

        let mut node = SearchNode {
            state_hash: StateHash::from_parts(0, hash_lo),
            beam_rank_score: 1234.0,
            turn: 1,
            action_count: 1,
            fast_eval: fast,
            ..Default::default()
        };
        node.set_full_eval(&full);
        arena.push_node(node)
    }

    #[test]
    fn tie_break_choice_lookahead_mixed_is_strict_total_order() {
        let mut arena = NodeArena::default();
        let mut ids = Vec::with_capacity(32);

        // Exact 3-node cycle witness that panicked without the ha vs hb guard
        ids.push(make_tied_node(&mut arena, 100, true, Some(200.0)));
        ids.push(make_tied_node(&mut arena, 50, false, None));
        ids.push(make_tied_node(&mut arena, 10, true, Some(100.0)));

        // Mix of has_choice_lookahead true/false with reversed/shuffled hashes and scores
        for i in 0..29u64 {
            let has_la = (i % 2) == 0;
            let hash_lo = ((29 - i) * 17) ^ 0x55;
            let rollout = if has_la {
                Some(((i * 37) % 13) as f64 * 10.0)
            } else {
                Some(999.0)
            };
            ids.push(make_tied_node(&mut arena, hash_lo, has_la, rollout));
        }

        for &a in &ids {
            assert_eq!(cmp_primary_desc(&arena, a, a, false), std::cmp::Ordering::Equal);
            for &b in &ids {
                let ab = cmp_primary_desc(&arena, a, b, false);
                let ba = cmp_primary_desc(&arena, b, a, false);
                assert_eq!(ab, ba.reverse(), "antisymmetry violated for {a:?} vs {b:?}");
                if !ab.is_gt() {
                    for &c in &ids {
                        let bc = cmp_primary_desc(&arena, b, c, false);
                        if !bc.is_gt() {
                            let ac = cmp_primary_desc(&arena, a, c, false);
                            assert!(
                                !ac.is_gt(),
                                "transitivity violated for {a:?} <= {b:?} <= {c:?}"
                            );
                        }
                    }
                }
            }
        }

        let root = RootEvalBaseline::default();
        let kept = BeamRetentionPolicy::prune_wave_full(
            &mut arena,
            &ids,
            16,
            None,
            None,
            &root,
            false,
            false,
            false,
            false,
            false,
        );
        assert_eq!(kept.len(), 16);
    }

    #[test]
    fn fast_partial_same_sig_potion_bucket_matches_full_sort() {
        let mut arena = NodeArena::default();
        let mut cands = Vec::with_capacity(220);
        for i in 0..220u64 {
            let mut full = StoredNodeEval::default();
            full.beam_rank_score = ((i.wrapping_mul(37)) % 97) as f64 * 13.0;
            full.effective_defense = (i % 19) as i32;
            full.board_investment = ((i * 5) % 17) as i32;
            full.setup_score = ((i * 3) % 23) as i32;
            full.optionality_cards = (i % 7) as i32;
            let mut fast = StoredFastEval::default();
            fast.cumulative_hp_lost = (i % 11) as i16;
            fast.enemy_hp = 100 - (i % 29) as i32;
            fast.alive_enemy_count = 1;
            let mut node = SearchNode {
                state_hash: StateHash::from_parts(0, i.wrapping_mul(17).wrapping_add(3)),
                beam_rank_score: full.beam_rank_score,
                turn: 1,
                action_count: 1,
                potion_count: 1,
                explicit_potion_count: 1,
                used_potion_slot_mask: 1,
                potion_strategic_cost: 9,
                fast_eval: fast,
                ..Default::default()
            };
            node.set_full_eval(&full);
            cands.push(arena.push_node(node));
        }

        let root = RootEvalBaseline {
            ignore_long_term_rewards: true,
            ..Default::default()
        };
        let mut scratch = PruneScratch::default();
        let mut actual_ids = Vec::new();
        BeamRetentionPolicy::prune_wave_full_into(
            &arena,
            &cands,
            32,
            None,
            None,
            &root,
            false,
            false,
            false,
            false,
            false,
            None,
            &mut actual_ids,
            &mut scratch,
        );

        let mut expected_scratch = PruneScratch::default();
        let bucket = &mut expected_scratch.buckets[1];
        bucket.nodes.extend_from_slice(&cands);
        for (i, &nid) in bucket.nodes.iter().enumerate() {
            let (k, _, _, _, _, _, sh) = arena.build_prune_entry(nid, &root, false);
            bucket.order.push((k.primary, i as u32));
            bucket.keys.push(k);
            bucket.state_hashes.push(sh);
        }
        bucket.order.sort_unstable_by(|&(pa, ia), &(pb, ib)| {
            pb.total_cmp(&pa).then_with(|| {
                cmp_key_tie_break(&bucket.keys, &bucket.state_hashes, ia as usize, ib as usize)
            })
        });
        assert_eq!(actual_ids.len(), 32);
        // Primary leader + 0c top pick guarantees top element matches `bucket.order[0]`
        assert_eq!(actual_ids[0], bucket.keys[bucket.order[0].1 as usize].node_id);

        // Compare directly against a second `prune_wave_full_into` call with mixed sigs (which takes full sort)
        let last = arena.get_mut(*cands.last().unwrap());
        last.used_potion_slot_mask = 2;
        last.beam_rank_score = -1_000_000.0;
        let mut expected_ids = Vec::new();
        BeamRetentionPolicy::prune_wave_full_into(
            &arena,
            &cands,
            32,
            None,
            None,
            &root,
            false,
            false,
            false,
            false,
            false,
            None,
            &mut expected_ids,
            &mut expected_scratch,
        );
        // Same-sig partial path picks 1 diverse slot (order[0]) then niches, while mixed path adds the worst-score slot-2 node
        assert_eq!(actual_ids[0], expected_ids[0]);
    }

    #[test]
    fn scalar_pick_top_k_niche_fast_handles_ties_and_extremes() {
        let root = RootEvalBaseline::default();
        for &n in &[0usize, 1, 15, 31, 32, 33, 64, 127, 257] {
            let mut keys = Vec::with_capacity(n);
            let mut hashes = Vec::with_capacity(n);
            let mut fast_keys = Vec::with_capacity(n);
            let mut init_marks = Vec::with_capacity(n);
            for i in 0..n {
                let primary = ((i * 17) % 13) as f64 * 100.0;
                let fk = match i % 11 {
                    0 => i64::MIN,
                    1 => i64::MAX,
                    2 => -1,
                    3 => 0,
                    4 => 1,
                    // Force frequent w_fast ties to test boundary tie-break parity
                    _ => ((i as i64 * 37) % 9) * 10 - 30,
                };
                let (mut k, _, _, _, _, _) = PruneKey::from_raw_fields(
                    NodeId(i as u32),
                    &root,
                    false,
                    (i % 50) as i32,
                    70,
                    80,
                    5,
                    65,
                    (i % 7) as i16,
                    0,
                    0,
                    1,
                    0,
                    0,
                    0,
                    0,
                    3,
                    0,
                    0,
                    0,
                    0,
                    0,
                    true,
                    primary,
                    false,
                    false,
                    0.0,
                    5,
                    1,
                    1,
                    1,
                );
                k.cumulative_hp_lost = ((i * 5) % 9) as i16;
                keys.push(k);
                hashes.push(StateHash::from_parts(0, ((n - i) as u64).wrapping_mul(31)));
                fast_keys.push(fk);
                init_marks.push(if i % 4 == 0 { 1u8 } else { 0u8 });
            }

            for &(take, pareto_extra) in &[(1usize, 0usize), (5, 4), (16, 16), (32, 32)] {
                let mut marks_scalar = init_marks.clone();
                let mut heap_scalar = Vec::new();
                let mut out_scalar = Vec::new();
                let mut p_scalar = [0usize; PER_NICHE_PARETO_TOP_K];
                let mut pc_scalar = 0usize;

                let added_scalar = pick_top_k_niche_fast_impl(
                    &keys,
                    &hashes,
                    &fast_keys,
                    &mut marks_scalar,
                    &mut heap_scalar,
                    take,
                    pareto_extra,
                    &mut out_scalar,
                    &mut p_scalar,
                    &mut pc_scalar,
                );
                let unmarked_total = init_marks.iter().filter(|&&m| m == 0).count();
                assert_eq!(added_scalar, take.min(unmarked_total));
                assert_eq!(
                    pc_scalar,
                    pareto_extra.min(unmarked_total.saturating_sub(added_scalar))
                );
            }
        }
    }

    #[test]
    fn frozen_incumbent_guards_match_csharp_hp_potion_and_turn_rules() {
        let mut root = RootEvalBaseline::default();
        root.initial_player_hp = 80;
        root.initial_player_max_hp = 80;
        root.boss_hp_relief = BossHpRelief::None;
        let cert = ClosureCertificate {
            healing_certified: true,
            max_in_combat_heal: 0,
            growth_certified_zero: true,
            ..Default::default()
        };

        let mut inc = FrozenIncumbentBound::default();
        // theft_preserve_resources disables tightening even when outstanding_stolen == 0 and !forfeits_gold
        assert!(!inc.tighten(&root, true, 0, -1000, 0, 10, 0, 0, 0, 3, 0, false, true));
        assert!(!inc.has_certified_overall_bound);

        // Zero-potion win at 70 HP (-1000 centi-HP), 10 battle_hp_lost, turn 3
        assert!(inc.tighten(&root, true, 0, -1000, 0, 10, 0, 0, 0, 3, 0, false, false));
        // Strictly lower battle_hp_lost on HP tie updates tie-break even on later turn
        assert!(inc.tighten(&root, true, 0, -1000, 0, 8, 0, 0, 0, 5, 0, false, false));
        assert_eq!(inc.best_zero_potion_battle_hp_lost, 8);
        assert_eq!(inc.best_zero_potion_end_turn, 5);

        // Zero-potion tie on upper_bound_hp_delta (-1000): fewer battle_hp_lost (7 < 8) at turn 6 > 5 must NOT be pruned
        assert!(!inc.is_pruned(&cert, &root, 70, 80, 0, 0, 0, false, false, 6, 7, 0, 0, 0, 0, false));
        // battle_hp_lost >= 8 at turn 6 > 5 IS pruned
        assert!(inc.is_pruned(&cert, &root, 70, 80, 0, 0, 0, false, false, 6, 8, 0, 0, 0, 0, false));
        // battle_hp_lost > 8 at turn 4 <= 5 is pruned by overall bound (`battle_hp_lost > best_overall_battle_hp_lost`)
        assert!(inc.is_pruned(&cert, &root, 70, 80, 0, 0, 0, false, false, 4, 9, 0, 0, 0, 0, false));
        // Overall V=-1000 tie with higher persistent HP (-100 > -1000 despite 900 potion cost) must NOT be pruned
        assert!(!inc.is_pruned(&cert, &root, 79, 80, 0, 0, 0, false, false, 4, 8, 1, 1, 1, 900, false));
        // Overall tie on V=-1000 AND hp_delta=-1000 AND battle_hp_lost==8 with potion_count 1 > 0 IS pruned even when turn <= 5
        assert!(inc.is_pruned(&cert, &root, 70, 80, 0, 0, 0, false, false, 4, 8, 1, 0, 0, 0, false));

        // Reset to isolate zero-potion vs overall `potion_count`:
        let mut inc2 = FrozenIncumbentBound::default();
        // 1-potion win: player_hp=79 (-100 centi-HP), potion_cost=900 -> V = -1000, hp_delta = -100, loss = 1, potions = 1, turn = 3
        assert!(inc2.tighten(&root, true, 0, -100, 900, 1, 1, 1, 1, 3, 0, false, false));
        // Another 1-potion route tying V=-1000, hp_delta=-100, loss=1, potions=0 < 1 must NOT be pruned
        assert!(!inc2.is_pruned(&cert, &root, 79, 80, 0, 0, 0, false, false, 5, 1, 0, 0, 1, 900, false));
        // Same potions=1 and loss=1, turn 4 > 3 IS pruned
        assert!(inc2.is_pruned(&cert, &root, 79, 80, 0, 0, 0, false, false, 4, 1, 1, 0, 1, 900, false));
    }

    #[test]
    fn tactical_sig_clone_capping_and_pass2_spillover_behavior() {
        let mut arena = NodeArena::default();
        let root = RootEvalBaseline {
            initial_player_hp: 80,
            initial_player_max_hp: 80,
            ignore_long_term_rewards: true,
            ..Default::default()
        };

        // 8 clones with identical tactical stats (only differ in state_hash and score 1000..993)
        let mut cands = Vec::new();
        for i in 0..8u64 {
            let mut n = SearchNode {
                state_hash: StateHash::from_parts(0, 100 + i),
                beam_rank_score: (1000 - i) as f64,
                turn: 1,
                action_count: 1,
                fast_eval: StoredFastEval {
                    player_hp: 70,
                    player_max_hp: 80,
                    enemy_hp: 50,
                    alive_enemy_count: 1,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut full = StoredNodeEval::default();
            full.beam_rank_score = n.beam_rank_score;
            n.set_full_eval(&full);
            cands.push(arena.push_node(n));
        }
        // 4 distinct tactical states with lower primary scores (different enemy_hp / block)
        for j in 0..4u64 {
            let mut n = SearchNode {
                state_hash: StateHash::from_parts(0, 200 + j),
                beam_rank_score: (500 - j) as f64,
                turn: 1,
                action_count: 1,
                fast_eval: StoredFastEval {
                    player_hp: 65 - j as i16,
                    player_max_hp: 80,
                    enemy_hp: 40 - j as i32 * 5,
                    alive_enemy_count: 1,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut full = StoredNodeEval::default();
            full.beam_rank_score = n.beam_rank_score;
            n.set_full_eval(&full);
            cands.push(arena.push_node(n));
        }

        // At turn boundary (cap=2 per tactical_sig), beam_width=8 selects 2 clones + 4 diverse + 2 spillover clones
        let mut kept_tb = Vec::new();
        let mut scratch = PruneScratch::default();
        BeamRetentionPolicy::prune_wave_full_into(
            &arena,
            &cands,
            8,
            None,
            None,
            &root,
            false,
            false,
            false,
            true,
            false,
            None,
            &mut kept_tb,
            &mut scratch,
        );
        assert_eq!(kept_tb.len(), 8);
        for d in &cands[8..12] {
            assert!(kept_tb.contains(d), "diverse node must survive Pass 1 clone cap");
        }

        // Mid-turn (cap=3 per tactical_sig) takes 3 clones + 4 diverse + 1 spillover = 8
        let kept_mt = BeamRetentionPolicy::prune_wave_full(
            &mut arena,
            &cands,
            8,
            None,
            None,
            &root,
            false,
            false,
            false,
            false,
            false,
        );
        assert_eq!(kept_mt.len(), 8);
        assert!(kept_mt.contains(&cands[2]), "3rd clone allowed under mid-turn cap=3");
        assert!(kept_mt.contains(&cands[11]), "diverse node 4 still retained");

        // All-clones narrow tree: Pass 2 spillover fills full quota cleanly
        let narrow = BeamRetentionPolicy::prune_wave_full(
            &mut arena,
            &cands[..8],
            6,
            None,
            None,
            &root,
            false,
            false,
            false,
            true,
            false,
        );
        assert_eq!(narrow.len(), 6);
    }

    #[test]
    fn potion_bucketing_optional_paid_forced_and_dynamic_quotas_match_csharp() {
        let mut root = RootEvalBaseline::default();
        root.initial_player_hp = 80;
        root.initial_player_max_hp = 80;
        root.forced_directive_count = 1;
        root.all_forced_potion_mask = 1u64 << 2;

        // Free/combat-generated potion (`potion_count=1`, `optional_paid=0`, `forced_mask=0`) lands in Bucket 0
        assert_eq!(compute_potion_bucket(1, 0, 0, 0, &root, false), 0);
        // 1 optional paid + 1 forced lands in Bucket 2
        assert_eq!(compute_potion_bucket(2, 2, 1, 1u64 << 2, &root, false), 2);
        // RequireAtLeastOne promotes explicit_potion_count > 0 to >= Bucket 1
        root.effective_potion_policy = SolverPotionPolicy::RequireAtLeastOne;
        assert_eq!(compute_potion_bucket(1, 1, 0, 0, &root, false), 1);

        // Phase B Bucket 0 dynamic reserves
        let counts = [100, 100, 100, 100];
        let q_phase_b_t1 = allocate_potion_bucket_quotas(counts, 100, true, false, 0, false, 1);
        assert_eq!(q_phase_b_t1[0], 30);
        let q_phase_b_t2 = allocate_potion_bucket_quotas(counts, 100, true, false, 0, false, 2);
        assert_eq!(q_phase_b_t2[0], 26);
        let q_relaxed = allocate_potion_bucket_quotas(counts, 100, true, true, 0, false, 1);
        assert_eq!(q_relaxed[0], 25);
        let q_floor40 = allocate_potion_bucket_quotas(counts, 100, true, false, 40, false, 2);
        assert_eq!(q_floor40[0], 40);
        let q_forced_t2 = allocate_potion_bucket_quotas(counts, 100, true, false, 0, true, 2);
        assert_eq!(q_forced_t2[0], 12);
        let q_phase_a_t1 = allocate_potion_bucket_quotas(counts, 100, false, false, 0, false, 1);
        assert_eq!(q_phase_a_t1[0], 60);
        assert_eq!(q_phase_a_t1.iter().sum::<usize>(), 100);
    }

    #[test]
    fn death_save_rescue_pareto_survival_and_strategy_niche_carve() {
        let mut arena = NodeArena::default();
        let mut root = RootEvalBaseline::default();
        root.initial_player_hp = 80;
        root.initial_player_max_hp = 80;
        root.effective_has_growth_targets = true;

        // Node A: survived cleanly at 10 HP, 0 death saves, high score
        let mut n_clean = SearchNode {
            state_hash: StateHash::from_parts(0, 1),
            beam_rank_score: 5000.0,
            turn: 2,
            action_count: 1,
            fast_eval: StoredFastEval {
                player_hp: 10,
                player_max_hp: 80,
                cumulative_hp_lost: 70,
                death_save_use_count: 0,
                enemy_hp: 50,
                alive_enemy_count: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut f_clean = StoredNodeEval::default();
        f_clean.beam_rank_score = 5000.0;
        n_clean.set_full_eval(&f_clean);

        // Node B: death-save rescue at 40 HP (same effective net loss if death_save_hp_restored=30 -> raw 80-40+30=70),
        // with high max_dark_evoke=30 so neither dominates unconditionally once cross-death-save domination is disabled.
        let mut n_rescue = SearchNode {
            state_hash: StateHash::from_parts(0, 2),
            beam_rank_score: 100.0,
            turn: 2,
            action_count: 2,
            max_dark_evoke: 30,
            fast_eval: StoredFastEval {
                player_hp: 40,
                player_max_hp: 80,
                cumulative_hp_lost: 40,
                death_save_hp_restored: 30,
                death_save_use_count: 1,
                enemy_hp: 50,
                alive_enemy_count: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut f_rescue = StoredNodeEval::default();
        f_rescue.beam_rank_score = 100.0;
        f_rescue.max_dark_evoke = 30;
        n_rescue.set_full_eval(&f_rescue);

        let id_clean = arena.push_node(n_clean);
        let id_rescue = arena.push_node(n_rescue);
        let (k_c, ..) = arena.build_prune_entry(id_clean, &root, false);
        let (k_r, ..) = arena.build_prune_entry(id_rescue, &root, false);
        let e_c = arena.build_extended_prune_entry(id_clean, &k_c, &root, false);
        let e_r = arena.build_extended_prune_entry(id_rescue, &k_r, &root, false);

        // Without rescue: DeathSave=0 < DeathSave=1 wins in Survival; if clean also matches dark evoke, clean dominates rescue in Pareto
        assert_eq!(cmp_entry_survival(&e_c, &e_r, false, false, false), std::cmp::Ordering::Less);
        // With rescue: higher PlayerHp (40 > 10) defers DeathSave and wins in Survival; and cross-death-save domination is disabled in Pareto
        assert_eq!(cmp_entry_survival(&e_r, &e_c, true, false, false), std::cmp::Ordering::Less);
        assert!(!dominates_candidate(&e_c, &e_r, true, false, 0, false, false, false));

        // Even if e_c beats e_r on every numeric stat including dark_evoke, `allow_death_save_rescue = true` still blocks cross-death-save domination
        let mut e_c_sup = e_c;
        e_c_sup.best_hp = 80;
        e_c_sup.max_dark_evoke = 99;
        assert!(dominates_candidate(&e_c_sup, &e_r, false, false, 0, false, false, false));
        assert!(!dominates_candidate(&e_c_sup, &e_r, true, false, 0, false, false, false));

        // Verify Strategy niche preserves strategy goal leader when `allow_death_save_rescue` / growth targets active
        let mut nodes = Vec::new();
        for i in 0..30u64 {
            let mut n = SearchNode {
                state_hash: StateHash::from_parts(0, 500 + i),
                beam_rank_score: (2000 - i) as f64,
                turn: 1,
                action_count: 1,
                board_investment: i as i16,
                fast_eval: StoredFastEval {
                    player_hp: 70 - (i % 5) as i16,
                    player_max_hp: 80,
                    enemy_hp: 50 + i as i32,
                    alive_enemy_count: 1,
                    strategy_goal_hp_credit: if i == 29 { 25 } else { 0 },
                    strategy_goal_count: if i == 29 { 2 } else { 0 },
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut full = StoredNodeEval::default();
            full.beam_rank_score = n.beam_rank_score;
            full.board_investment = i as i32;
            n.set_full_eval(&full);
            nodes.push(arena.push_node(n));
        }
        let mut kept = Vec::new();
        let mut scratch = PruneScratch {
            allow_death_save_rescue: true,
            ..Default::default()
        };
        BeamRetentionPolicy::prune_wave_full_into(
            &arena,
            &nodes,
            12,
            None,
            None,
            &root,
            false,
            false,
            false,
            true,
            false,
            None,
            &mut kept,
            &mut scratch,
        );
        assert!(kept.contains(&nodes[29]), "Strategy leader must be preserved by Strategy niche");

        // Verify BoardInvestment (16_000*board + 18_000*setup), mid-turn Optionality combo, and non-bypassed explicit split
        let mut bi_nodes = Vec::new();
        for i in 0..24u64 {
            let mut n = SearchNode {
                state_hash: StateHash::from_parts(0, 900 + i),
                beam_rank_score: (3000 - i) as f64,
                turn: 1,
                action_count: 1,
                explicit_potion_count: if i < 12 { 0 } else { 1 },
                optional_paid_potion_count: if i < 12 { 0 } else { 1 },
                potion_count: if i < 12 { 0 } else { 1 },
                board_investment: if i == 11 { 2 } else { 0 },
                setup_score: if i == 11 { 5 } else { 0 },
                fast_eval: StoredFastEval {
                    player_hp: 75,
                    player_max_hp: 80,
                    enemy_hp: 60,
                    energy: if i == 23 { 6 } else { 0 },
                    playable_hand_count: if i == 23 { 5 } else { 0 },
                    alive_enemy_count: 1,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut full = StoredNodeEval::default();
            full.beam_rank_score = n.beam_rank_score;
            full.board_investment = n.board_investment as i32;
            full.setup_score = n.setup_score as i32;
            n.set_full_eval(&full);
            bi_nodes.push(arena.push_node(n));
        }
        let kept_split = BeamRetentionPolicy::prune_wave_full(
            &mut arena,
            &bi_nodes,
            10,
            Some(5),
            Some(5),
            &root,
            false,
            false,
            false,
            false,
            false,
        );
        assert_eq!(kept_split.len(), 10);
        // Bucket 0 must run BoardInvestment and keep bi_nodes[11]; Bucket 1 must run Optionality combo and keep bi_nodes[23]!
        assert!(kept_split.contains(&bi_nodes[11]), "explicit split must preserve BoardInvestment leader in Bucket 0");
        assert!(kept_split.contains(&bi_nodes[23]), "explicit split must preserve mid-turn Optionality combo leader in Bucket 1");
    }
}
