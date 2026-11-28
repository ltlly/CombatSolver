use crate::arena::{NodeArena, NodeId};
use crate::hash::StateHash;
use crate::packer::{pack_state, unpack_state};
use crate::tt::TranspositionLabel;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use std::borrow::Cow;
use std::sync::atomic::{fence, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::OnceLock;
use sts2_kernel::state::{
    hot_keys, BattleState, CardInst, CardTempFlags, CreatureInst, ENEMY_POWER_FLAG_GOLD_THEFT,
    ENEMY_POWER_FLAG_REVIVING, FieldValue, GrowthCounters, HOOK_DIRTY_CREATURES,
};
use sts2_kernel::{CardHandle, CardRolloutProfile, CombatEngine, ContentDispatcher, CreatureHandle};
#[cfg(debug_assertions)]
use sts2_types::Dec;
use sts2_types::{
    is_ascension_active_in_mode, AfflictionId, AscensionLevel, CardId, CardKeywords, CardMeta,
    CardRarity, CardStaticFlags, CardTags, CardType, CombatSide, GameMode, MonsterId, OrbId,
    PileType, PlayerRngType, PotionId, PowerId, PowerMeta, PowerStackType, PowerStaticFlags,
    PowerType, RelicId, RelicStaticFlags, RoomType,
};

pub mod weights {
    pub const DEATH_PENALTY: f64 = -1_000_000_000_000.0;
    pub const VICTORY_BONUS: f64 = 10_000_000_000.0;
    pub const HP: f64 = 30_000.0;
    pub const ENEMY_HP: f64 = -10_000.0;
    pub const RISK_PENALTY: f64 = -2_000.0;
    pub const ACTION_PENALTY: f64 = -1.0;

    pub const VULNERABLE_ATTACK_WINDOW_CAP: i32 = 24;
    pub const VULNERABLE_ATTACK_MULTIPLIER_BEAM_VALUE: f64 = 5_000.0;
    pub const OFF_TARGET_VULNERABLE_ATTACK_MULTIPLIER_BEAM_VALUE: f64 = 1_000.0;

    pub const CURRENT_ENERGY_BEAM_CAP: i32 = 6;
    pub const CURRENT_ENERGY_BEAM_VALUE: f64 = 60_000.0;

    pub const PERSISTENT_BUFF_DELTA_BEAM_CAP: i32 = 32;
    pub const PERSISTENT_BUFF_DELTA_BEAM_VALUE: f64 = 50_000.0;
    pub const STANDARD_PERSISTENT_BUFF_DELTA_BEAM_CAP: i32 = 4;
    pub const STANDARD_PERSISTENT_BUFF_DELTA_BEAM_VALUE: f64 = 150_000.0;

    pub const LATENT_SETUP_BEAM_CAP: i32 = 24;
    pub const LATENT_SETUP_BEAM_VALUE: f64 = 12_000.0;

    pub const REPLAY_POTENTIAL_BEAM_CAP: i32 = 64;
    pub const REPLAY_POTENTIAL_BEAM_VALUE: f64 = 10_000.0;

    pub const LONG_TERM_RESOURCE_BEAM_VALUE: f64 = 25_000.0;
    pub const LONG_TERM_RESOURCE_BEAM_CAP: f64 = 25_000.0;

    pub const MINIMUM_TURN_LAYER_EXPANDED_NODES: usize = 500;
    pub const NO_VICTORY_ESCALATION_FACTOR: usize = 2;
    pub const MAXIMUM_NO_VICTORY_ESCALATIONS: usize = 2;
    pub const MAXIMUM_ESCALATED_BEAM_WIDTH: usize = 512;
    pub const MAXIMUM_ESCALATED_BRANCHES_PER_ACTION: usize = 100;

    pub const ANGER_COPY_BEAM_PENALTY: f64 = -15_000.0;
    pub const RETAINED_ATTACK_GROWTH_BEAM_CAP: i32 = 16;
    pub const RETAINED_ATTACK_GROWTH_BEAM_VALUE: f64 = 20_000.0;
    pub const FUTURE_RESOURCE_BEAM_VALUE: f64 = 10_000.0;
    pub const DELAYED_DAMAGE_BEAM_VALUE: f64 = 10_000.0;
    pub const SANDPIT_TURN_BEAM_VALUE: f64 = 30_000.0;

    pub const ENEMY_STRENGTH_SUPPRESSION_BEAM_CAP: i32 = 16;
    pub const ENEMY_WEAK_TURNS_BEAM_CAP: i32 = 16;
    pub const STANDARD_ENEMY_STRENGTH_SUPPRESSION_HORIZON: i32 = 4;
    pub const BOSS_ENEMY_STRENGTH_SUPPRESSION_HORIZON: i32 = 8;
    pub const STANDARD_ENEMY_WEAK_EXPECTED_HP_SAVED: i32 = 1;
    pub const BOSS_ENEMY_WEAK_EXPECTED_HP_SAVED: i32 = 2;

    pub const LIVE_DECK_CLUTTER_PENALTY: f64 = -8_000.0;
    pub const OUTSTANDING_STOLEN_RESOURCE_PENALTY: f64 = -1_000_000.0;
    pub const SOLD_HP_PENALTY: f64 = -20_000.0;

    pub const POTION_MINIMUM_HP_SAVED: i32 = 9;
    pub const POTION_HIGH_VALUE_HP_SAVED: i32 = POTION_MINIMUM_HP_SAVED * 2;
    pub const POTION_ELEVATED_VALUE_HP_SAVED: i32 = (POTION_MINIMUM_HP_SAVED * 3 + 1) / 2;
    pub const HP_FIXED_SCALE: i32 = 100;
    pub const MAX_HP_GAIN_WEIGHT_CENTI: i32 = 200;
    pub const MAX_HP_LOSS_WEIGHT_CENTI: i32 = 100;
    pub const RUN_ENDING_POTION_TIE_BREAK_CENTI: i32 = 50;
    pub const REPLACED_POTION_FLOOR_CENTI: i32 = 50;
    pub const DEATH_SAVE_PREMIUM_PERCENT: i32 = 900;

    pub const SETUP_VALUE_HORIZON_TURNS: i32 = 16;
    pub const MAXIMUM_SEARCH_MAX_DEGREE_OF_PARALLELISM: usize = 16;
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum BossHpStrategy {
    #[default]
    ProgressionFirst,
    MinimizeHpLoss,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum SolverPotionDirective {
    #[default]
    Smart,
    Force,
    Disabled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PotionSlotDirective {
    pub slot: usize,
    pub potion_id: PotionId,
    pub directive: SolverPotionDirective,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[repr(u8)]
pub enum RelicCounterId {
    HappyFlower = 0,
    FakeHappyFlower = 1,
    Pendulum = 2,
    PollinousCore = 3,
    PenNib = 4,
    Nunchaku = 5,
    TuningFork = 6,
    JossPaper = 7,
    IronClub = 8,
    GalacticDust = 9,
    MeatOnTheBone = 10,
}

impl RelicCounterId {
    pub fn from_str_name(s: &str) -> Option<Self> {
        match s {
            "HappyFlower" => Some(Self::HappyFlower),
            "FakeHappyFlower" => Some(Self::FakeHappyFlower),
            "Pendulum" => Some(Self::Pendulum),
            "PollinousCore" => Some(Self::PollinousCore),
            "PenNib" => Some(Self::PenNib),
            "Nunchaku" => Some(Self::Nunchaku),
            "TuningFork" => Some(Self::TuningFork),
            "JossPaper" => Some(Self::JossPaper),
            "IronClub" => Some(Self::IronClub),
            "GalacticDust" => Some(Self::GalacticDust),
            "MeatOnTheBone" => Some(Self::MeatOnTheBone),
            _ => None,
        }
    }

    pub const fn period(self) -> i32 {
        match self {
            Self::HappyFlower | Self::Pendulum => 3,
            Self::FakeHappyFlower | Self::JossPaper => 5,
            Self::PollinousCore | Self::IronClub => 4,
            Self::PenNib | Self::Nunchaku | Self::TuningFork | Self::GalacticDust => 10,
            Self::MeatOnTheBone => 2,
        }
    }

    pub const fn relic_id(self) -> RelicId {
        match self {
            Self::HappyFlower => RelicId::HappyFlower,
            Self::FakeHappyFlower => RelicId::FakeHappyFlower,
            Self::Pendulum => RelicId::Pendulum,
            Self::PollinousCore => RelicId::PollinousCore,
            Self::PenNib => RelicId::PenNib,
            Self::Nunchaku => RelicId::Nunchaku,
            Self::TuningFork => RelicId::TuningFork,
            Self::JossPaper => RelicId::JossPaper,
            Self::IronClub => RelicId::IronClub,
            Self::GalacticDust => RelicId::GalacticDust,
            Self::MeatOnTheBone => RelicId::MeatOnTheBone,
        }
    }

    pub fn read_counter(self, state: &BattleState) -> i32 {
        let read_field = |relic_id: RelicId, key: &str| -> i32 {
            state
                .player
                .relics
                .iter()
                .find(|r| !r.is_melted && r.id == relic_id)
                .and_then(|r| r.private_fields.get(key))
                .and_then(|v| match v {
                    FieldValue::Int(x) => Some(*x),
                    _ => None,
                })
                .unwrap_or(0)
        };
        let raw = match self {
            Self::HappyFlower => read_field(RelicId::HappyFlower, "HappyFlower._turnCounter"),
            Self::FakeHappyFlower => {
                read_field(RelicId::FakeHappyFlower, "FakeHappyFlower._turnsSeen")
            }
            Self::Pendulum => read_field(RelicId::Pendulum, "Pendulum._turnCounter"),
            Self::PollinousCore => read_field(RelicId::PollinousCore, "PollinousCore._handsSeen"),
            Self::PenNib => read_field(RelicId::PenNib, hot_keys::PEN_NIB_ATTACKS_PLAYED),
            Self::Nunchaku => read_field(RelicId::Nunchaku, hot_keys::NUNCHAKU_ATTACKS_PLAYED),
            Self::TuningFork => read_field(RelicId::TuningFork, "TuningFork._skillsPlayed"),
            Self::JossPaper => read_field(RelicId::JossPaper, "JossPaper._cardsExhausted"),
            Self::IronClub => read_field(RelicId::IronClub, "IronClub._cardsPlayed"),
            Self::GalacticDust => read_field(RelicId::GalacticDust, "GalacticDust._starsSpent"),
            Self::MeatOnTheBone => {
                let pc = state.player_creature();
                let hp = pc.map_or(state.player.current_hp, |c| c.current_hp);
                let max_hp = pc.map_or(state.player.max_hp, |c| c.max_hp);
                i32::from(hp * 100 <= max_hp * 50)
            }
        };
        raw.rem_euclid(self.period())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelicCounterRule {
    pub id: RelicCounterId,
    pub enabled: bool,
    pub minimum: i32,
    pub maximum: i32,
    pub hp_allowance: i32,
    pub priority: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RelicCounterTarget {
    pub id: RelicCounterId,
    pub minimum: i32,
    pub maximum: i32,
    pub hp_allowance: i32,
    pub period: i32,
    pub priority: i32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RelicCounterEvaluation {
    pub satisfied_mask: u64,
    pub target_mask: u64,
    pub hp_credit: i32,
    pub healing_hp_credit: i32,
    pub distance: i32,
    pub satisfied_priority: i32,
}

impl RelicCounterEvaluation {
    #[inline]
    pub fn all_satisfied(&self) -> bool {
        self.satisfied_mask == self.target_mask
    }

    #[inline]
    pub fn total_hp_credit(&self) -> i32 {
        self.hp_credit.saturating_add(self.healing_hp_credit)
    }
}

pub struct RelicCounterPolicy;

impl RelicCounterPolicy {
    pub fn capture_targets(
        state: &BattleState,
        enabled: bool,
        rules: &[RelicCounterRule],
    ) -> Vec<RelicCounterTarget> {
        if !enabled {
            return Vec::new();
        }
        let mut targets: Vec<RelicCounterTarget> = rules
            .iter()
            .filter(|r| {
                r.enabled
                    && state
                        .player
                        .relics
                        .iter()
                        .any(|owned| !owned.is_melted && owned.id == r.id.relic_id())
            })
            .filter_map(|r| {
                let period = r.id.period();
                if r.minimum > 0 || r.maximum < period - 1 {
                    Some(RelicCounterTarget {
                        id: r.id,
                        minimum: r.minimum,
                        maximum: r.maximum,
                        hp_allowance: r.hp_allowance,
                        period,
                        priority: r.priority,
                    })
                } else {
                    None
                }
            })
            .collect();
        targets.sort_by_key(|t| t.id);
        targets
    }

    pub fn evaluate(
        state: &BattleState,
        targets: &[RelicCounterTarget],
        won: bool,
        post_combat_heal: PostCombatRelicHealProfile,
        strategic_boss_hp_relief: BossHpRelief,
    ) -> RelicCounterEvaluation {
        if targets.is_empty() {
            return RelicCounterEvaluation::default();
        }
        let mut eval = RelicCounterEvaluation::default();
        for &target in targets {
            let value = target.id.read_counter(state);
            let bit = 1u64 << (target.id as u32);
            let satisfied = value >= target.minimum && value <= target.maximum;
            let reward_goal = satisfied && target.id != RelicCounterId::MeatOnTheBone;
            let distance = if satisfied {
                0
            } else if value < target.minimum {
                target.minimum - value
            } else {
                target.period - value + target.minimum
            };
            eval.target_mask |= bit;
            if satisfied {
                eval.satisfied_mask |= bit;
            }
            if reward_goal {
                eval.hp_credit = eval.hp_credit.saturating_add(target.hp_allowance);
                eval.satisfied_priority = eval
                    .satisfied_priority
                    .saturating_add(1i32 << ((target.priority.clamp(1, 3) - 1) * 4));
            }
            eval.distance = eval.distance.saturating_add(distance);
        }
        if won
            && (eval.satisfied_mask & (1u64 << (RelicCounterId::MeatOnTheBone as u32))) != 0
        {
            let pc = state.player_creature();
            let hp = pc.map_or(state.player.current_hp, |c| c.current_hp);
            let max_hp = pc.map_or(state.player.max_hp, |c| c.max_hp);
            let threshold_heal = post_combat_heal.heal_for(hp, max_hp)
                - post_combat_heal.monotone_heal_for(hp, max_hp);
            eval.healing_hp_credit = ActEndingBossPolicy::persistent_value_of_recovered_hp(
                threshold_heal,
                strategic_boss_hp_relief,
            );
        }
        eval
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GrowthOpportunityTargets {
    pub required_rewards: GrowthCounters,
    pub is_bounded: bool,
    pub has_targets: bool,
}

impl GrowthOpportunityTargets {
    pub fn is_satisfied_by(&self, rewards: &GrowthCounters) -> bool {
        self.is_bounded && rewards.satisfies(&self.required_rewards)
    }

    pub fn capture(state: &BattleState, content: &dyn ContentDispatcher) -> Self {
        use sts2_kernel::state::CardInst;
        use sts2_types::EnchantmentId;

        let is_mad_science_improvement = |c: &CardInst| -> bool {
            if c.id != CardId::MadScience {
                return false;
            }
            let Some(ext) = state.card_ext(c.ext_index) else {
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
        };

        let is_goopy = |c: &CardInst| -> bool {
            c.deck_card_handle.is_some()
                && state
                    .card_ext(c.ext_index)
                    .and_then(|e| e.enchantment.as_ref())
                    .is_some_and(|en| en.id == EnchantmentId::Goopy)
        };

        let is_target_card = |c: &CardInst| -> bool {
            matches!(
                c.id,
                CardId::HandOfGreed
                    | CardId::TheHunt
                    | CardId::Feed
                    | CardId::Royalties
                    | CardId::Alchemize
                    | CardId::ForbiddenGrimoire
            ) || is_mad_science_improvement(c)
                || (c.deck_card_handle.is_some()
                    && matches!(
                        c.id,
                        CardId::GeneticAlgorithm | CardId::TheScythe | CardId::ShowOff
                    ))
                || is_goopy(c)
        };

        let available: Vec<&CardInst> = state
            .cards
            .iter()
            .filter(|c| !c.removed_from_state && c.pile != PileType::Exhaust && c.pile != PileType::Deck)
            .collect();

        let ms_cap = mad_science_upgrade_capacity(state, content);
        let has_any = available.iter().any(|c| is_target_card(c));
        if !has_any {
            return Self {
                required_rewards: GrowthCounters::default(),
                is_bounded: true,
                has_targets: false,
            };
        }

        let has_replay_or_copy_card = available.iter().any(|c| {
            matches!(
                c.id,
                CardId::Burst
                    | CardId::EchoForm
                    | CardId::OneTwoPunch
                    | CardId::SignalBoost
                    | CardId::TagTeam
                    | CardId::KiVerse
                    | CardId::Nightmare
                    | CardId::DualWield
                    | CardId::HeirloomHammer
                    | CardId::Juggling
                    | CardId::ImitationLearning
                    | CardId::HowlFromBeyond
            )
        });
        let has_replay_power = state.player_creature().is_some_and(|pc| {
            pc.powers.iter().any(|p| {
                p.amount > 0
                    && matches!(
                        p.id,
                        PowerId::BurstPower
                            | PowerId::DuplicationPower
                            | PowerId::EchoFormPower
                            | PowerId::OneTwoPunchPower
                            | PowerId::SignalBoostPower
                            | PowerId::TagTeamPower
                            | PowerId::JugglingPower
                            | PowerId::ImitationLearningPower
                            | PowerId::NightmarePower
                            | PowerId::KiVersePower
                    )
            })
        });
        let has_replay_relic = state.player.relics.iter().any(|r| {
            !r.is_melted && matches!(r.id, RelicId::ThrowingAxe | RelicId::BurningSticks)
        });
        let has_dup_potion = state
            .player
            .potion_slots
            .iter()
            .any(|p| *p == Some(PotionId::Duplicator));

        if has_replay_or_copy_card || has_replay_power || has_replay_relic || has_dup_potion {
            return Self {
                required_rewards: GrowthCounters::default(),
                is_bounded: false,
                has_targets: true,
            };
        }

        let enemy_count = state
            .enemy_handles
            .iter()
            .filter(|&&h| state.creature(h).is_some_and(|c| c.is_alive() && state.contains_creature(h)))
            .count() as i32;

        let mut required = GrowthCounters::default();
        let has_hog = available.iter().any(|c| c.id == CardId::HandOfGreed);
        let has_hunt = available.iter().any(|c| c.id == CardId::TheHunt);
        let has_feed = available.iter().any(|c| c.id == CardId::Feed);
        let fatal_kinds = usize::from(has_hog) + usize::from(has_hunt) + usize::from(has_feed);
        if fatal_kinds > 1 {
            return Self {
                required_rewards: GrowthCounters::default(),
                is_bounded: false,
                has_targets: true,
            };
        } else if has_hog {
            required.hand_of_greed = enemy_count;
        } else if has_hunt {
            let n = available.iter().filter(|c| c.id == CardId::TheHunt).count() as i32;
            required.the_hunt = enemy_count.min(n);
        } else if has_feed {
            let n = available.iter().filter(|c| c.id == CardId::Feed).count() as i32;
            required.feed = enemy_count.min(n);
        }

        let count_plays = |pred: &dyn Fn(&CardInst) -> bool| -> i32 {
            available
                .iter()
                .filter(|c| pred(c))
                .map(|c| 1 + (c.base_replay_count as i32).max(0))
                .sum()
        };

        required.royalties = count_plays(&|c| c.id == CardId::Royalties);
        required.alchemize = count_plays(&|c| c.id == CardId::Alchemize);
        required.genetic_algorithm =
            count_plays(&|c| c.id == CardId::GeneticAlgorithm && c.deck_card_handle.is_some());
        required.the_scythe =
            count_plays(&|c| c.id == CardId::TheScythe && c.deck_card_handle.is_some());
        required.goopy = count_plays(&is_goopy);
        required.forbidden_grimoire = count_plays(&|c| c.id == CardId::ForbiddenGrimoire);
        required.mad_science = ms_cap.min(count_plays(&is_mad_science_improvement));

        let show_off_plays =
            count_plays(&|c| c.id == CardId::ShowOff && c.deck_card_handle.is_some())
                + state
                    .player_creature()
                    .map_or(0, |pc| {
                        pc.powers
                            .iter()
                            .filter(|p| p.id == PowerId::ShowOffPower && p.amount > 0)
                            .count() as i32
                    });

        let has_targets = required.total() > 0 || show_off_plays > 0;
        Self {
            required_rewards: required,
            is_bounded: show_off_plays == 0,
            has_targets,
        }
    }
}

pub fn mad_science_upgrade_capacity(state: &BattleState, content: &dyn ContentDispatcher) -> i32 {
    let upgradable = state
        .player
        .deck
        .iter()
        .filter(|&&h| {
            state.card(h).is_some_and(|c| {
                content
                    .card_meta(c.id)
                    .is_ok_and(|m| c.upgrade_level < m.max_upgrade_level)
            })
        })
        .count() as i32;
    let committed: i32 = state.player_creature().map_or(0, |pc| {
        pc.powers
            .iter()
            .filter(|p| p.id == PowerId::ImprovementPower && p.amount > 0)
            .map(|p| p.amount)
            .sum()
    });
    (upgradable - committed).max(0)
}

#[inline]
pub fn allows_brightest_flame(limit: Option<i32>, spent_at_root: i32, spent_after_action: i32) -> bool {
    match limit {
        None => true,
        Some(cap) => spent_after_action <= cap.max(spent_at_root),
    }
}

pub fn predict_potion_replacement_hp_credit(
    state: &BattleState,
    policy: &crate::SearchPolicy,
) -> i32 {
    let belt_full = !state.player.potion_slots.is_empty()
        && state.player.potion_slots.iter().all(|s| s.is_some());
    let procure_blocked = state
        .player
        .relics
        .iter()
        .any(|r| !r.is_melted && r.id == RelicId::Sozu);
    let is_final_boss = state.static_ctx.room_type == RoomType::Boss
        && state.static_ctx.act_index >= 2
        && !policy.is_non_final_act3_boss;
    if !belt_full || procure_blocked || is_final_boss {
        return 0;
    }
    let room = state.static_ctx.room_type;
    if !matches!(room, RoomType::Monster | RoomType::Elite | RoomType::Boss) {
        return 0;
    }
    let has_white_beast = state
        .player
        .relics
        .iter()
        .any(|r| !r.is_melted && r.id == RelicId::WhiteBeastStatue);
    if policy.predict_potion_reward {
        if let Some(credit) = policy.potion_reward_replacement_hp_credit {
            return credit.max(0);
        }
        let forced = has_white_beast
            || (state.static_ctx.game_mode == GameMode::Blitz
                && (room == RoomType::Boss
                    || (room == RoomType::Monster && !state.static_ctx.encounter_is_weak)));
        if forced {
            return weights::POTION_MINIMUM_HP_SAVED;
        }
        let chance =
            (0.40f32 + if room == RoomType::Elite { 0.125 } else { 0.0 }).clamp(0.0, 1.0);
        let mut rng = *state.player_rngs.get(PlayerRngType::Rewards);
        return if rng.random.next_float() < chance {
            weights::POTION_MINIMUM_HP_SAVED
        } else {
            0
        };
    }
    if has_white_beast {
        weights::POTION_MINIMUM_HP_SAVED
    } else {
        0
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum BossHpRelief {
    #[default]
    None,
    ActClearHeal,
    RunEnding,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum SolverPotionPolicy {
    Disabled,
    RequireAtLeastOne,
    #[default]
    Smart,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum SolverTheftPolicy {
    #[default]
    Ignore,
    PreserveResources,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PostCombatRelicHealProfile {
    pub unconditional_heal: i32,
    pub wounded_heal: i32,
    pub wounded_hp_percent: i32,
}

impl PostCombatRelicHealProfile {
    pub fn from_state(state: &BattleState) -> Self {
        let mut unconditional_heal = 0i32;
        let mut wounded_heal = 0i32;
        let mut wounded_hp_percent = 0i32;
        for r in &state.player.relics {
            if r.is_melted {
                continue;
            }
            match r.id {
                RelicId::BurningBlood => unconditional_heal += 6,
                RelicId::BlackBlood => unconditional_heal += 12,
                RelicId::MeatOnTheBone => {
                    wounded_heal += 12;
                    wounded_hp_percent = 50;
                }
                _ => {}
            }
        }
        Self {
            unconditional_heal,
            wounded_heal,
            wounded_hp_percent,
        }
    }

    #[inline]
    pub fn heal_for(self, final_hp: i32, final_max_hp: i32) -> i32 {
        let mut capacity = self.unconditional_heal.max(0);
        if self.wounded_heal > 0 && final_hp <= (final_max_hp.max(0) * self.wounded_hp_percent.max(0)) / 100 {
            capacity += self.wounded_heal;
        }
        capacity.clamp(0, (final_max_hp - final_hp).max(0))
    }

    #[inline]
    pub fn monotone_heal_for(self, final_hp: i32, final_max_hp: i32) -> i32 {
        self.unconditional_heal
            .max(0)
            .clamp(0, (final_max_hp - final_hp).max(0))
    }
}

pub struct ActEndingBossPolicy;

impl ActEndingBossPolicy {
    #[inline]
    pub fn resolve_strategic_hp_relief(
        encounter_relief: BossHpRelief,
        act_transition_strategy: BossHpStrategy,
        final_boss_strategy: BossHpStrategy,
    ) -> BossHpRelief {
        match encounter_relief {
            BossHpRelief::ActClearHeal if act_transition_strategy == BossHpStrategy::MinimizeHpLoss => {
                BossHpRelief::None
            }
            BossHpRelief::RunEnding if final_boss_strategy == BossHpStrategy::MinimizeHpLoss => {
                BossHpRelief::None
            }
            other => other,
        }
    }

    #[inline]
    pub const fn persistent_hp_scale(relief: BossHpRelief) -> i32 {
        match relief {
            BossHpRelief::ActClearHeal => 35,
            BossHpRelief::RunEnding => 20,
            BossHpRelief::None => 100,
        }
    }

    #[inline]
    pub const fn round_centi_to_hp(centi: i32) -> i32 {
        if centi >= 0 {
            (centi + 50) / weights::HP_FIXED_SCALE
        } else {
            -((-centi + 50) / weights::HP_FIXED_SCALE)
        }
    }

    #[allow(clippy::too_many_arguments)]
    #[inline]
    pub fn compute_net_persistent_hp_delta_centi(
        end_player_hp: i32,
        post_combat_monotone_heal: i32,
        initial_player_hp: i32,
        end_player_max_hp: i32,
        initial_player_max_hp: i32,
        death_save_hp_restored: i32,
        growth_hp_credit: i32,
        boss_hp_relief: BossHpRelief,
        ignore_long_term_rewards: bool,
        feed_max_hp_excluded: i32,
    ) -> i32 {
        let hp_scale = Self::persistent_hp_scale(boss_hp_relief);
        let death_save_hp = death_save_hp_restored.max(0);
        let raw_hp_delta =
            (end_player_hp + post_combat_monotone_heal - death_save_hp) - initial_player_hp;
        let max_hp_gain = (end_player_max_hp - initial_player_max_hp).max(0);
        let non_feed_max_hp_gain = (max_hp_gain - feed_max_hp_excluded.max(0)).max(0);
        let max_hp_loss = (initial_player_max_hp - end_player_max_hp).max(0);

        let max_hp_gain_credit_centi =
            if boss_hp_relief != BossHpRelief::RunEnding && !ignore_long_term_rewards {
                non_feed_max_hp_gain.saturating_mul(weights::MAX_HP_GAIN_WEIGHT_CENTI)
            } else {
                0
            };
        let max_hp_loss_penalty_centi = if boss_hp_relief != BossHpRelief::RunEnding {
            max_hp_loss.saturating_mul(weights::MAX_HP_LOSS_WEIGHT_CENTI)
        } else {
            0
        };
        let death_save_penalty_centi =
            Self::death_save_premium(death_save_hp).saturating_mul(weights::HP_FIXED_SCALE);

        raw_hp_delta
            .saturating_mul(hp_scale)
            .saturating_add(max_hp_gain_credit_centi)
            .saturating_sub(max_hp_loss_penalty_centi)
            .saturating_add(growth_hp_credit.saturating_mul(weights::HP_FIXED_SCALE))
            .saturating_sub(death_save_penalty_centi)
    }

    #[inline]
    pub fn raw_hp_required_for_persistent_value(persistent_hp_value: i32, relief: BossHpRelief) -> i32 {
        if persistent_hp_value <= 0 {
            return 0;
        }
        match relief {
            BossHpRelief::ActClearHeal => persistent_hp_value.saturating_mul(5),
            BossHpRelief::RunEnding => i32::MAX / 4,
            BossHpRelief::None => persistent_hp_value,
        }
    }

    #[inline]
    pub fn persistent_value_of_recovered_hp(recovered_hp: i32, relief: BossHpRelief) -> i32 {
        if recovered_hp <= 0 {
            return 0;
        }
        let denom = Self::raw_hp_required_for_persistent_value(1, relief);
        if denom <= 0 {
            0
        } else {
            recovered_hp / denom
        }
    }

    #[inline]
    pub fn death_save_premium(death_save_hp_restored: i32) -> i32 {
        death_save_hp_restored.max(0) * weights::DEATH_SAVE_PREMIUM_PERCENT / 100
    }

    #[inline]
    pub fn death_save_beam_cost(death_save_hp_restored: i32) -> i32 {
        let r = death_save_hp_restored.max(0);
        r + Self::death_save_premium(r)
    }

    #[inline]
    pub fn strategic_hp_deficit(
        cumulative_hp_lost: i32,
        max_hp_deficit: i32,
        recovered_hp: i32,
        relief: BossHpRelief,
        death_save_hp_restored: i32,
    ) -> i32 {
        cumulative_hp_lost + max_hp_deficit
            - Self::persistent_value_of_recovered_hp(recovered_hp - death_save_hp_restored, relief)
            + Self::death_save_premium(death_save_hp_restored)
    }

    #[inline]
    pub fn effective_optional_potion_burden(
        optional_potion_count: i32,
        optional_potion_strategic_cost: i32,
        relief: BossHpRelief,
    ) -> i32 {
        if optional_potion_count <= 0 || optional_potion_strategic_cost <= 0 {
            return 0;
        }
        match relief {
            BossHpRelief::RunEnding => 0,
            BossHpRelief::ActClearHeal => optional_potion_strategic_cost.saturating_mul(5),
            BossHpRelief::None => optional_potion_strategic_cost,
        }
    }

    #[inline]
    pub fn ranked_post_combat_relic_heal(
        profile: PostCombatRelicHealProfile,
        complete_victory: bool,
        final_hp: i32,
        final_max_hp: i32,
    ) -> i32 {
        if complete_victory && final_hp > 0 {
            profile.monotone_heal_for(final_hp, final_max_hp)
        } else {
            0
        }
    }
}

pub struct PotionValuationRegistry;

impl PotionValuationRegistry {
    #[inline]
    pub fn strategic_hp_cost(id: PotionId, renewable_potion_shaped_rock: bool) -> i32 {
        if id == PotionId::PotionShapedRock
            || (renewable_potion_shaped_rock && id == PotionId::PotionShapedRock)
        {
            return 0;
        }
        match id {
            PotionId::Ambergris
            | PotionId::GlowwaterPotion
            | PotionId::SwiftPotion
            | PotionId::GamblersBrew
            | PotionId::Duplicator
            | PotionId::OrobicAcid
            | PotionId::PotOfGhouls => weights::POTION_HIGH_VALUE_HP_SAVED,
            PotionId::DistilledChaos
            | PotionId::Clarity
            | PotionId::RadiantTincture
            | PotionId::CureAll
            | PotionId::LiquidMemories
            | PotionId::BottledPotential
            | PotionId::TouchOfInsanity => weights::POTION_ELEVATED_VALUE_HP_SAVED,
            _ => weights::POTION_MINIMUM_HP_SAVED,
        }
    }

    #[inline]
    pub fn strategic_hp_cost_with_overrides(
        id: PotionId,
        renewable_potion_shaped_rock: bool,
        overrides: &std::collections::BTreeMap<PotionId, i32>,
    ) -> i32 {
        if let Some(&cost) = overrides.get(&id) {
            return cost.clamp(0, 100);
        }
        Self::strategic_hp_cost(id, renewable_potion_shaped_rock)
    }

    #[inline]
    pub fn strategic_hp_cost_for_root(
        id: PotionId,
        renewable_potion_shaped_rock: bool,
        initial_player_max_hp: i32,
        overrides: &std::collections::BTreeMap<PotionId, i32>,
    ) -> i32 {
        if let Some(&cost) = overrides.get(&id) {
            return cost.clamp(0, 100);
        }
        if id == PotionId::PotionShapedRock
            || (renewable_potion_shaped_rock && id == PotionId::PotionShapedRock)
        {
            return 0;
        }
        if id == PotionId::Ambergris {
            let max_hp = initial_player_max_hp.max(1);
            let ceil_40 = (max_hp * 4 + 9) / 10;
            return weights::POTION_HIGH_VALUE_HP_SAVED.max(ceil_40);
        }
        if id == PotionId::BloodPotion {
            if initial_player_max_hp <= 0 {
                return weights::POTION_MINIMUM_HP_SAVED;
            }
            return weights::POTION_MINIMUM_HP_SAVED.max((initial_player_max_hp * 20) / 100 + 1);
        }
        Self::strategic_hp_cost(id, false)
    }

    #[inline]
    pub fn requires_opening_use(id: PotionId) -> bool {
        matches!(
            id,
            PotionId::DexterityPotion
                | PotionId::FocusPotion
                | PotionId::FyshOil
                | PotionId::LiquidBronze
                | PotionId::MazalethsGift
                | PotionId::PotionOfCapacity
                | PotionId::SoldiersStew
                | PotionId::StrengthPotion
        )
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PaidPotionImproveInput {
    pub won: bool,
    pub survives: bool,
    pub death_save_use_count: i32,
    pub end_player_hp: i32,
    pub end_player_max_hp: i32,
    pub used_potion_slot_mask: u16,
    pub optional_paid_potion_count: u8,
    pub raw_potion_strategic_cost: i32,
    pub net_potion_opportunity_cost_centi: i32,
    pub alchemize_count: u8,
    pub empty_potion_slots: u8,
    pub all_strategy_targets_satisfied: bool,
    pub all_growth_targets_satisfied: bool,
    pub growth_hp_credit: i32,
    pub relic_satisfied_mask: u32,
    pub outstanding_stolen: i32,
    pub forfeits_gold: bool,
    pub theft_preserve_resources: bool,
}

pub struct PotionUsePolicy;

impl PotionUsePolicy {
    #[inline]
    pub fn compute_net_potion_opportunity_cost_centi(
        optional_paid_raw_hp_cost: i32,
        optional_paid_count: i32,
        alchemize_count: i32,
        end_empty_potion_slots: i32,
        root: &RootEvalBaseline,
    ) -> i32 {
        if optional_paid_count <= 0 || optional_paid_raw_hp_cost <= 0 {
            return 0;
        }
        if root.encounter_boss_hp_relief == BossHpRelief::RunEnding {
            return optional_paid_count.saturating_mul(weights::RUN_ENDING_POTION_TIE_BREAK_CENTI);
        }
        let raw_centi = optional_paid_raw_hp_cost.saturating_mul(weights::HP_FIXED_SCALE);
        let mut replacement_centi = 0i32;
        if !root.has_sozu {
            let drop_lands = root.belt_full_at_root
                && root.base_replacement_credit_centi > 0
                && (end_empty_potion_slots > 0 || alchemize_count == 0);
            let mut remaining_slots = optional_paid_count;
            if drop_lands {
                replacement_centi += root.base_replacement_credit_centi;
                remaining_slots -= 1;
            }
            let alchemize_refills = remaining_slots
                .min((alchemize_count - i32::from(root.empty_root_potion_slots)).max(0));
            if alchemize_refills > 0 {
                // Subtract Alchemize growth budget already credited in NetPersistentHpDeltaCenti
                // so a refilled slot is never double-counted past default Standard potion value.
                let alchemize_per_refill_hp =
                    (weights::POTION_MINIMUM_HP_SAVED - root.effective_growth_budgets.alchemize).max(0);
                if alchemize_per_refill_hp > 0 {
                    replacement_centi += alchemize_refills
                        .saturating_mul(alchemize_per_refill_hp * weights::HP_FIXED_SCALE);
                }
            }
        }
        let after_replacement_centi = raw_centi - replacement_centi;
        if after_replacement_centi <= weights::REPLACED_POTION_FLOOR_CENTI {
            weights::REPLACED_POTION_FLOOR_CENTI
        } else {
            after_replacement_centi
        }
    }

    #[inline]
    pub fn can_any_paid_potion_improve_on(
        input: PaidPotionImproveInput,
        root: &RootEvalBaseline,
    ) -> bool {
        let remaining_paid_mask = root.paid_optional_slot_mask & !input.used_potion_slot_mask;
        if root.paid_optional_potion_count == 0 || remaining_paid_mask == 0 {
            return false;
        }
        if !input.won || !input.survives || input.death_save_use_count > 0 {
            return true;
        }
        if input.theft_preserve_resources && (input.outstanding_stolen > 0 || input.forfeits_gold) {
            return true;
        }

        let hp_scale = ActEndingBossPolicy::persistent_hp_scale(root.boss_hp_relief);

        // Price the cheapest remaining unused paid slot (falls back to root minimum if slot costs unavailable).
        let mut min_remaining_raw_cost = i32::MAX;
        let slot_limit =
            usize::from(root.root_potion_slot_count).min(root.root_slot_costs.len());
        for slot in 0..slot_limit {
            if (remaining_paid_mask & (1u16 << slot)) == 0 {
                continue;
            }
            let slot_cost = i32::from(root.root_slot_costs[slot]);
            if slot_cost > 0 && slot_cost < min_remaining_raw_cost {
                min_remaining_raw_cost = slot_cost;
            }
        }
        if min_remaining_raw_cost == i32::MAX {
            min_remaining_raw_cost = root.min_paid_potion_cost;
        }

        // Swapping an already-spent costlier paid slot for a cheaper unused paid slot can improve V.
        if input.optional_paid_potion_count > 0
            && input.raw_potion_strategic_cost > min_remaining_raw_cost
        {
            return true;
        }

        // Incremental opportunity cost when another paid potion has already been spent (avoids double-counting
        // full-belt replacement credit).
        let min_alchemize_refill = i32::from(root.has_alchemize_in_deck && !root.has_sozu);
        let min_net_cost_centi = if input.optional_paid_potion_count > 0 {
            let cur_net_cost_centi = input.net_potion_opportunity_cost_centi;
            let next_total_net_centi = Self::compute_net_potion_opportunity_cost_centi(
                input
                    .raw_potion_strategic_cost
                    .saturating_add(min_remaining_raw_cost),
                i32::from(input.optional_paid_potion_count) + 1,
                i32::from(input.alchemize_count) + min_alchemize_refill,
                (i32::from(input.empty_potion_slots) + 1).max(1),
                root,
            );
            (next_total_net_centi - cur_net_cost_centi).max(0)
        } else {
            Self::compute_net_potion_opportunity_cost_centi(
                min_remaining_raw_cost,
                1,
                min_alchemize_refill,
                1,
                root,
            )
        };

        let has_remaining_heal = (remaining_paid_mask & root.paid_heal_potion_slot_mask) != 0;
        let has_remaining_fruit_juice =
            (remaining_paid_mask & root.paid_fruit_juice_slot_mask) != 0;
        let has_remaining_entropic =
            (remaining_paid_mask & root.paid_entropic_brew_slot_mask) != 0;

        let end_max_hp = if input.end_player_max_hp > 0 {
            input.end_player_max_hp
        } else {
            root.initial_player_max_hp
        };
        let end_post_heal_hp = input.end_player_hp
            + ActEndingBossPolicy::ranked_post_combat_relic_heal(
                root.post_combat_relic_heal,
                input.won,
                input.end_player_hp,
                end_max_hp,
            );

        let mut remaining_fruit_juice_max_hp_gain = 0i32;
        if root.has_root_potion_metadata && (has_remaining_heal || has_remaining_fruit_juice) {
            for slot in 0..slot_limit {
                if (remaining_paid_mask & (1u16 << slot)) == 0 {
                    continue;
                }
                if root.root_potion_slot_ids[slot] == Some(PotionId::FruitJuice) {
                    remaining_fruit_juice_max_hp_gain += 5;
                }
            }
        }

        let mut remaining_heal_cap = 0i32;
        let mut has_unbounded_heal =
            has_remaining_entropic || root.has_any_heal_or_unknown_generation;
        if !has_unbounded_heal && has_remaining_heal {
            if !root.has_root_potion_metadata {
                has_unbounded_heal = true;
            } else {
                let max_hp_upper_bound =
                    root.initial_player_max_hp + remaining_fruit_juice_max_hp_gain;
                let mut added_regen_stacks = 0i32;
                for slot in 0..slot_limit {
                    let bit = 1u16 << slot;
                    if (remaining_paid_mask & root.paid_heal_potion_slot_mask & bit) == 0 {
                        continue;
                    }
                    match root.root_potion_slot_ids[slot] {
                        Some(PotionId::BloodPotion) => {
                            remaining_heal_cap += (max_hp_upper_bound * 20) / 100;
                        }
                        Some(PotionId::Ambergris) => {
                            remaining_heal_cap += (max_hp_upper_bound * 50) / 100;
                        }
                        Some(PotionId::RegenPotion) => {
                            added_regen_stacks += 5;
                        }
                        Some(PotionId::FruitJuice) => {}
                        _ => {
                            has_unbounded_heal = true;
                            break;
                        }
                    }
                }
                if !has_unbounded_heal && added_regen_stacks > 0 {
                    let total_regen = root.root_regen_stacks + added_regen_stacks;
                    remaining_heal_cap += ((total_regen * (total_regen + 1)) / 2)
                        - ((root.root_regen_stacks * (root.root_regen_stacks + 1)) / 2);
                }
            }
        }

        let max_raw_hp_gain = if has_unbounded_heal {
            (root.initial_player_max_hp - end_post_heal_hp).max(0)
        } else {
            let mut best_unhealed_end_hp = root
                .initial_player_max_hp
                .min(root.initial_player_hp + root.post_combat_relic_heal.unconditional_heal);
            if root.post_combat_relic_heal.wounded_heal > 0 {
                let wounded_cap_hp = root.initial_player_max_hp.min(
                    (root.initial_player_max_hp
                        * root.post_combat_relic_heal.wounded_hp_percent)
                        / 100
                        + root.post_combat_relic_heal.wounded_heal,
                );
                if wounded_cap_hp > best_unhealed_end_hp {
                    best_unhealed_end_hp = wounded_cap_hp;
                }
            }
            let best_reachable_end_hp = root
                .initial_player_max_hp
                .min(best_unhealed_end_hp + remaining_heal_cap);
            (best_reachable_end_hp - end_post_heal_hp).max(0)
        };

        let mut max_possible_gain_centi = (max_raw_hp_gain as i64) * (hp_scale as i64);

        let max_hp_loss = (root.initial_player_max_hp - end_max_hp).max(0);
        if max_hp_loss > 0 && root.boss_hp_relief != BossHpRelief::RunEnding {
            max_possible_gain_centi +=
                (max_hp_loss as i64) * (weights::MAX_HP_LOSS_WEIGHT_CENTI as i64);
        }

        if has_remaining_fruit_juice || has_remaining_entropic {
            let mut fj_gain_hp = if remaining_fruit_juice_max_hp_gain > 0 {
                remaining_fruit_juice_max_hp_gain
            } else {
                5
            };
            if has_remaining_entropic {
                let slots = i32::from(root.root_potion_slot_count).max(1);
                fj_gain_hp = fj_gain_hp.max(5 * slots);
            }
            let max_hp_gain_bonus = if root.boss_hp_relief != BossHpRelief::RunEnding
                && !root.ignore_long_term_rewards
            {
                fj_gain_hp * weights::MAX_HP_GAIN_WEIGHT_CENTI
            } else {
                0
            };
            max_possible_gain_centi +=
                (fj_gain_hp as i64) * (hp_scale as i64) + (max_hp_gain_bonus as i64);
        }

        let has_any_strategy_targets =
            root.effective_has_growth_targets || !root.active_relic_targets.is_empty();
        if has_any_strategy_targets && !input.all_strategy_targets_satisfied {
            if root.effective_has_growth_targets
                && !input.all_growth_targets_satisfied
                && !root.effective_growth_budgets.is_zero()
            {
                if !root.growth_opportunity_targets.is_bounded {
                    return true;
                }
                let max_growth_credit_hp = root
                    .effective_growth_budgets
                    .credit(&root.growth_opportunity_targets.required_rewards)
                    as i64;
                let unrealized_growth_hp =
                    (max_growth_credit_hp - (input.growth_hp_credit as i64)).max(0);
                max_possible_gain_centi +=
                    unrealized_growth_hp * (weights::HP_FIXED_SCALE as i64);
            }

            let all_relic_satisfied = root.active_relic_targets.iter().all(|t| {
                (input.relic_satisfied_mask & (1u32 << (t.id as u32))) != 0
            });
            if !root.active_relic_targets.is_empty() && !all_relic_satisfied {
                let mut unrealized_relic_hp = 0i64;
                for t in &root.active_relic_targets {
                    if (input.relic_satisfied_mask & (1u32 << (t.id as u32))) != 0 {
                        continue;
                    }
                    if t.id == RelicCounterId::MeatOnTheBone {
                        unrealized_relic_hp += i64::from(
                            ActEndingBossPolicy::persistent_value_of_recovered_hp(
                                root.post_combat_relic_heal.wounded_heal,
                                root.boss_hp_relief,
                            ),
                        );
                    } else if t.hp_allowance > 0 {
                        unrealized_relic_hp += i64::from(t.hp_allowance);
                    }
                }
                max_possible_gain_centi += unrealized_relic_hp * (weights::HP_FIXED_SCALE as i64);
            }
        }

        max_possible_gain_centi >= (min_net_cost_centi as i64)
    }

    #[inline]
    pub fn additional_required_use_strategic_hp_cost(strategic_hp_cost: i32) -> i32 {
        (strategic_hp_cost - weights::POTION_MINIMUM_HP_SAVED).max(0)
    }

    #[inline]
    pub fn apply_replacement_credit(
        optional_potion_strategic_cost: i32,
        optional_potion_count: i32,
        replacement_hp_credit: i32,
    ) -> i32 {
        if optional_potion_strategic_cost > 0
            && optional_potion_count > 0
            && replacement_hp_credit > 0
        {
            (optional_potion_strategic_cost - replacement_hp_credit).max(1)
        } else {
            optional_potion_strategic_cost
        }
    }

    #[inline]
    pub fn hp_saved(potion_free_hp_deficit: i32, potion_route_hp_deficit: i32) -> i32 {
        (potion_free_hp_deficit - potion_route_hp_deficit).max(0)
    }

    #[inline]
    pub fn smart_required_hp_saved(strategic_hp_cost: i32, relief: BossHpRelief) -> i32 {
        ActEndingBossPolicy::raw_hp_required_for_persistent_value(strategic_hp_cost, relief)
    }

    #[inline]
    pub fn ambergris_required_hp_saved(maximum_hp: i32) -> i32 {
        // Math.Ceiling(maximumHp * 0.40m) == (maximumHp * 4 + 9) / 10
        if maximum_hp <= 0 {
            0
        } else {
            (maximum_hp * 4 + 9) / 10
        }
    }

    #[inline]
    pub fn effective_strategic_hp_cost(
        strategic_hp_cost: i32,
        ambergris_count: i32,
        maximum_hp: i32,
    ) -> i32 {
        strategic_hp_cost
            + ambergris_count
                * (Self::ambergris_required_hp_saved(maximum_hp) - weights::POTION_MINIMUM_HP_SAVED)
    }

    #[inline]
    pub fn meets_ambergris_restriction(
        has_potion_free_baseline: bool,
        ambergris_count: i32,
        strategic_hp_cost: i32,
        maximum_hp: i32,
        potion_free_player_hp: i32,
        potion_route_player_hp: i32,
    ) -> bool {
        if ambergris_count == 0 {
            return true;
        }
        if !has_potion_free_baseline {
            return false;
        }
        let required =
            Self::effective_strategic_hp_cost(strategic_hp_cost, ambergris_count, maximum_hp);
        (potion_route_player_hp - potion_free_player_hp).max(0) >= required
    }

    #[allow(clippy::too_many_arguments)]
    #[inline]
    pub fn is_eligible(
        policy: SolverPotionPolicy,
        explicit_potion_count: i32,
        strategic_hp_cost: i32,
        potion_free_won: bool,
        potion_free_hp_deficit: i32,
        any_route_won: bool,
        potion_route_won: bool,
        potion_route_hp_deficit: i32,
    ) -> bool {
        match policy {
            SolverPotionPolicy::Disabled => explicit_potion_count == 0,
            SolverPotionPolicy::RequireAtLeastOne => {
                explicit_potion_count > 0
                    && (!any_route_won || potion_route_won)
                    && (explicit_potion_count == 1
                        || !potion_free_won
                        || Self::hp_saved(potion_free_hp_deficit, potion_route_hp_deficit)
                            >= Self::additional_required_use_strategic_hp_cost(strategic_hp_cost))
            }
            SolverPotionPolicy::Smart => {
                explicit_potion_count == 0
                    || (potion_route_won && !potion_free_won)
                    || Self::hp_saved(potion_free_hp_deficit, potion_route_hp_deficit)
                        >= Self::smart_required_hp_saved(strategic_hp_cost, BossHpRelief::None)
            }
        }
    }
}

/// Power card valuation commitment priorities matching `PowerCardValuationContracts.cs`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum PowerRoutePriority {
    #[default]
    Low = 0,
    Normal = 1,
    Strong = 2,
    Core = 3,
    Dedicated = 4,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PowerRouteAdmissionPolicy {
    pub priority: PowerRoutePriority,
    pub minimum_projection: i32,
}

pub struct PowerCardValuationRegistry;

impl PowerCardValuationRegistry {
    pub fn contains_card_id(id: CardId) -> bool {
        Self::admission(id).is_some()
    }

    pub fn admission(id: CardId) -> Option<PowerRouteAdmissionPolicy> {
        match id {
            CardId::EchoForm | CardId::Corruption | CardId::BiasedCognition => {
                Some(PowerRouteAdmissionPolicy {
                    priority: PowerRoutePriority::Dedicated,
                    minimum_projection: 1,
                })
            }
            CardId::Afterimage | CardId::DarkEmbrace | CardId::FeelNoPain | CardId::Barricade => {
                Some(PowerRouteAdmissionPolicy {
                    priority: PowerRoutePriority::Core,
                    minimum_projection: 1,
                })
            }
            CardId::DemonForm | CardId::Defragment | CardId::Footwork | CardId::Inflame
            | CardId::Accuracy | CardId::NoxiousFumes | CardId::Buffer => {
                Some(PowerRouteAdmissionPolicy {
                    priority: PowerRoutePriority::Strong,
                    minimum_projection: 1,
                })
            }
            CardId::InfiniteBlades | CardId::Juggernaut | CardId::Rupture | CardId::HelloWorld
            | CardId::Loop | CardId::ToolsOfTheTrade | CardId::WellLaidPlans => {
                Some(PowerRouteAdmissionPolicy {
                    priority: PowerRoutePriority::Normal,
                    minimum_projection: 1,
                })
            }
            _ => None,
        }
    }

    pub fn has_reachable_power(state: &BattleState) -> bool {
        state
            .cards
            .iter()
            .any(|c| !c.removed_from_state && c.pile != PileType::Exhaust && Self::contains_card_id(c.id))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RouteQuality {
    pub won: bool,
    pub survives: bool,
    pub is_event_defeat: bool,
    pub death_save_use_count: i32,
    pub outstanding_stolen_resource: i32,
    pub strategic_hp_deficit: i32,
    pub growth_hp_credit: i32,
    pub growth_reward_count: i32,
    pub combat_ended_turn: Option<i32>,
    pub projected_battle_hp_lost: i32,
    pub potion_strategic_cost: i32,
    pub projected_battle_potion_count: i32,
    pub enemy_hp: i32,
    pub score: f64,
    pub theft_policy: Option<SolverTheftPolicy>,
    pub forfeits_encounter_gold: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RouteQualityProjection {
    Primary,
    Interim,
    PotionPolicy,
}

pub struct RouteQualityPolicy;

impl RouteQualityPolicy {
    #[inline]
    pub fn is_complete_victory(
        action_count: usize,
        all_enemies_dead: bool,
        player_dead: bool,
        projected_player_hp: i32,
    ) -> bool {
        action_count > 0 && all_enemies_dead && !player_dead && projected_player_hp > 0
    }

    #[inline]
    pub fn is_resource_trade_improvement(
        candidate_hp_deficit: i32,
        candidate_potion_cost: i32,
        current_hp_deficit: i32,
        current_potion_cost: i32,
    ) -> bool {
        let candidate_burden = candidate_hp_deficit.saturating_add(candidate_potion_cost);
        let current_burden = current_hp_deficit.saturating_add(current_potion_cost);
        candidate_burden < current_burden
            || (candidate_burden == current_burden && candidate_hp_deficit < current_hp_deficit)
    }

    pub fn compare(
        candidate: &RouteQuality,
        current: &RouteQuality,
        projection: RouteQualityProjection,
    ) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        if projection == RouteQualityProjection::Primary {
            return Self::compare_primary(candidate, current);
        }

        let cmp = current.won.cmp(&candidate.won);
        if cmp != Ordering::Equal {
            return cmp;
        }
        let cmp = current.survives.cmp(&candidate.survives);
        if cmp != Ordering::Equal {
            return cmp;
        }
        let cmp = candidate.is_event_defeat.cmp(&current.is_event_defeat);
        if cmp != Ordering::Equal {
            return cmp;
        }
        let cmp = candidate.death_save_use_count.cmp(&current.death_save_use_count);
        if cmp != Ordering::Equal {
            return cmp;
        }

        if projection == RouteQualityProjection::Interim {
            if Self::is_resource_trade_improvement(
                candidate.strategic_hp_deficit,
                candidate.potion_strategic_cost,
                current.strategic_hp_deficit,
                current.potion_strategic_cost,
            ) {
                return Ordering::Less;
            }
            if Self::is_resource_trade_improvement(
                current.strategic_hp_deficit,
                current.potion_strategic_cost,
                candidate.strategic_hp_deficit,
                candidate.potion_strategic_cost,
            ) {
                return Ordering::Greater;
            }
            let cmp = current.growth_hp_credit.cmp(&candidate.growth_hp_credit);
            if cmp != Ordering::Equal {
                return cmp;
            }
            let cmp = current.growth_reward_count.cmp(&candidate.growth_reward_count);
            if cmp != Ordering::Equal {
                return cmp;
            }
            if candidate.strategic_hp_deficit == current.strategic_hp_deficit
                && candidate.potion_strategic_cost == current.potion_strategic_cost
                && candidate.projected_battle_potion_count == current.projected_battle_potion_count
                && candidate.projected_battle_hp_lost != current.projected_battle_hp_lost
            {
                return candidate
                    .projected_battle_hp_lost
                    .cmp(&current.projected_battle_hp_lost);
            }
            let cmp = candidate
                .combat_ended_turn
                .unwrap_or(i32::MAX)
                .cmp(&current.combat_ended_turn.unwrap_or(i32::MAX));
            if cmp != Ordering::Equal {
                return cmp;
            }
            let cmp = candidate
                .projected_battle_potion_count
                .cmp(&current.projected_battle_potion_count);
            if cmp != Ordering::Equal {
                return cmp;
            }
            let cmp = candidate.enemy_hp.cmp(&current.enemy_hp);
            return if cmp != Ordering::Equal {
                cmp
            } else {
                Self::compare_score(candidate.score, current.score)
            };
        }

        if projection == RouteQualityProjection::PotionPolicy {
            let candidate_cost =
                candidate.strategic_hp_deficit as i64 + candidate.potion_strategic_cost as i64;
            let current_cost =
                current.strategic_hp_deficit as i64 + current.potion_strategic_cost as i64;
            let cmp = candidate_cost.cmp(&current_cost);
            if cmp != Ordering::Equal {
                return cmp;
            }
            if candidate.won
                && current.won
                && candidate.strategic_hp_deficit == current.strategic_hp_deficit
                && candidate.potion_strategic_cost == current.potion_strategic_cost
                && candidate.projected_battle_potion_count == current.projected_battle_potion_count
                && candidate.growth_hp_credit == current.growth_hp_credit
                && candidate.growth_reward_count == current.growth_reward_count
                && candidate.projected_battle_hp_lost != current.projected_battle_hp_lost
            {
                return candidate
                    .projected_battle_hp_lost
                    .cmp(&current.projected_battle_hp_lost);
            }
        }

        let cmp = Self::compare_primary(candidate, current);
        if cmp != Ordering::Equal {
            return cmp;
        }
        let cmp = candidate
            .projected_battle_potion_count
            .cmp(&current.projected_battle_potion_count);
        if cmp != Ordering::Equal {
            cmp
        } else {
            Self::compare_score(candidate.score, current.score)
        }
    }

    #[inline]
    pub fn compare_primary(candidate: &RouteQuality, current: &RouteQuality) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        let cmp = current.won.cmp(&candidate.won);
        if cmp != Ordering::Equal {
            return cmp;
        }
        let cmp = candidate.death_save_use_count.cmp(&current.death_save_use_count);
        if cmp != Ordering::Equal {
            return cmp;
        }
        let cmp = candidate.strategic_hp_deficit.cmp(&current.strategic_hp_deficit);
        if cmp != Ordering::Equal {
            return cmp;
        }
        let cmp = current.growth_hp_credit.cmp(&candidate.growth_hp_credit);
        if cmp != Ordering::Equal {
            return cmp;
        }
        let cmp = current.growth_reward_count.cmp(&candidate.growth_reward_count);
        if cmp != Ordering::Equal {
            return cmp;
        }
        let cmp = candidate
            .projected_battle_hp_lost
            .cmp(&current.projected_battle_hp_lost);
        if cmp != Ordering::Equal {
            return cmp;
        }
        let cmp = candidate
            .projected_battle_potion_count
            .cmp(&current.projected_battle_potion_count);
        if cmp != Ordering::Equal {
            return cmp;
        }
        candidate
            .combat_ended_turn
            .unwrap_or(i32::MAX)
            .cmp(&current.combat_ended_turn.unwrap_or(i32::MAX))
    }

    #[inline]
    fn compare_score(candidate: f64, current: f64) -> std::cmp::Ordering {
        if candidate > current {
            std::cmp::Ordering::Less
        } else if current > candidate {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RareNodeEval {
    pub growth_rewards: GrowthCounters,
    pub relic_eval: RelicCounterEvaluation,
}

impl RareNodeEval {
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.growth_rewards.is_zero() && self.relic_eval.target_mask == 0
    }
}

pub const FAST_FLAG_PLAYER_DEAD: u8 = 1 << 0;
pub const FAST_FLAG_ALL_ENEMIES_DEAD: u8 = 1 << 1;
pub const FAST_FLAG_WON: u8 = 1 << 2;
pub const FAST_FLAG_HAS_PENDING_CHOICE: u8 = 1 << 3;
pub const FAST_FLAG_FORFEITS_GOLD: u8 = 1 << 4;
pub const FAST_FLAG_EVENT_DEFEAT: u8 = 1 << 5;
pub const FAST_FLAG_ALL_STRATEGY_TARGETS_SATISFIED: u8 = 1 << 6;

/// Compact 40-byte per-node summary stored inline inside `SearchNode`.
/// Infrequent `growth_rewards` and `relic_eval` spill to `NodeArena::rare_evals`.
#[derive(Clone, Copy, Debug)]
pub struct StoredFastEval {
    pub enemy_hp: i32,
    pub player_hp: i16,
    pub player_max_hp: i16,
    pub player_block: i16,
    pub projected_player_hp: i16,
    pub cumulative_hp_lost: i16,
    pub recovered_hp: i16,
    pub death_save_hp_restored: i16,
    pub strategy_goal_hp_credit: i16,
    pub strategic_hp_credit: i16,
    pub energy: i16,
    pub stars: i16,
    pub outstanding_stolen: i16,
    pub combat_ended_turn_encoded: u16,
    pub alive_enemy_count: u8,
    pub reviving_enemy_count: u8,
    pub death_save_use_count: u8,
    pub raw_growth_reward_count: u8,
    pub strategy_goal_count: u8,
    pub alchemize_count: u8,
    pub empty_potion_slots: u8,
    pub feed_count: u8,
    pub playable_hand_count: u8,
    pub flags: u8,
}

const _: () = assert!(std::mem::size_of::<StoredFastEval>() <= 40);

impl Default for StoredFastEval {
    #[inline]
    fn default() -> Self {
        Self {
            enemy_hp: 0,
            player_hp: 0,
            player_max_hp: 0,
            player_block: 0,
            projected_player_hp: 0,
            cumulative_hp_lost: 0,
            recovered_hp: 0,
            death_save_hp_restored: 0,
            strategy_goal_hp_credit: 0,
            strategic_hp_credit: 0,
            energy: 0,
            stars: 0,
            outstanding_stolen: 0,
            combat_ended_turn_encoded: u16::MAX,
            alive_enemy_count: 0,
            reviving_enemy_count: 0,
            death_save_use_count: 0,
            raw_growth_reward_count: 0,
            strategy_goal_count: 0,
            alchemize_count: 0,
            empty_potion_slots: 0,
            feed_count: 0,
            playable_hand_count: 0,
            flags: 0,
        }
    }
}

impl From<&FastEvalSummary> for StoredFastEval {
    #[inline]
    fn from(f: &FastEvalSummary) -> Self {
        let mut flags = 0u8;
        if f.player_dead {
            flags |= FAST_FLAG_PLAYER_DEAD;
        }
        if f.all_enemies_dead {
            flags |= FAST_FLAG_ALL_ENEMIES_DEAD;
        }
        if f.won {
            flags |= FAST_FLAG_WON;
        }
        if f.has_pending_choice {
            flags |= FAST_FLAG_HAS_PENDING_CHOICE;
        }
        if f.forfeits_gold {
            flags |= FAST_FLAG_FORFEITS_GOLD;
        }
        if f.is_event_defeat {
            flags |= FAST_FLAG_EVENT_DEFEAT;
        }
        if f.all_strategy_targets_satisfied {
            flags |= FAST_FLAG_ALL_STRATEGY_TARGETS_SATISFIED;
        }
        Self {
            enemy_hp: f.enemy_hp,
            player_hp: f.player_hp.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            player_max_hp: f.player_max_hp.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            player_block: f.player_block.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            projected_player_hp: f.projected_player_hp.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            cumulative_hp_lost: f.cumulative_hp_lost.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            recovered_hp: f.recovered_hp.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            death_save_hp_restored: f.death_save_hp_restored.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            strategy_goal_hp_credit: f.strategy_goal_hp_credit.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            strategic_hp_credit: f.strategic_hp_credit.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            energy: f.energy.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            stars: f.stars.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            outstanding_stolen: f.outstanding_stolen.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            combat_ended_turn_encoded: match f.combat_ended_turn {
                Some(t) => t.clamp(0, (u16::MAX - 1) as i32) as u16,
                None => u16::MAX,
            },
            alive_enemy_count: f.alive_enemy_count.clamp(0, u8::MAX as i32) as u8,
            reviving_enemy_count: f.reviving_enemy_count.clamp(0, u8::MAX as i32) as u8,
            death_save_use_count: f.death_save_use_count.clamp(0, u8::MAX as i32) as u8,
            raw_growth_reward_count: f.raw_growth_reward_count.clamp(0, u8::MAX as i32) as u8,
            strategy_goal_count: f.strategy_goal_count.clamp(0, u8::MAX as i32) as u8,
            alchemize_count: f.alchemize_count,
            empty_potion_slots: f.empty_potion_slots,
            feed_count: f.feed_count,
            playable_hand_count: f.playable_hand_count,
            flags,
        }
    }
}

impl StoredFastEval {
    #[inline(always)]
    pub const fn player_dead(&self) -> bool {
        (self.flags & FAST_FLAG_PLAYER_DEAD) != 0
    }

    #[inline(always)]
    pub const fn all_enemies_dead(&self) -> bool {
        (self.flags & FAST_FLAG_ALL_ENEMIES_DEAD) != 0
    }

    #[inline(always)]
    pub const fn won(&self) -> bool {
        (self.flags & FAST_FLAG_WON) != 0
    }

    #[inline(always)]
    pub const fn has_pending_choice(&self) -> bool {
        (self.flags & FAST_FLAG_HAS_PENDING_CHOICE) != 0
    }

    #[inline(always)]
    pub const fn forfeits_gold(&self) -> bool {
        (self.flags & FAST_FLAG_FORFEITS_GOLD) != 0
    }

    #[inline(always)]
    pub const fn is_event_defeat(&self) -> bool {
        (self.flags & FAST_FLAG_EVENT_DEFEAT) != 0
    }

    #[inline(always)]
    pub const fn all_strategy_targets_satisfied(&self) -> bool {
        (self.flags & FAST_FLAG_ALL_STRATEGY_TARGETS_SATISFIED) != 0
    }

    #[inline(always)]
    pub const fn combat_ended_turn(&self) -> Option<i32> {
        if self.combat_ended_turn_encoded == u16::MAX {
            None
        } else {
            Some(self.combat_ended_turn_encoded as i32)
        }
    }

    #[inline(always)]
    pub fn set_won(&mut self, v: bool) {
        if v {
            self.flags |= FAST_FLAG_WON | FAST_FLAG_ALL_ENEMIES_DEAD;
        } else {
            self.flags &= !FAST_FLAG_WON;
        }
    }

    #[inline(always)]
    pub fn set_player_dead(&mut self, v: bool) {
        if v {
            self.flags |= FAST_FLAG_PLAYER_DEAD;
        } else {
            self.flags &= !FAST_FLAG_PLAYER_DEAD;
        }
    }

    #[inline(always)]
    pub fn set_forfeits_gold(&mut self, v: bool) {
        if v {
            self.flags |= FAST_FLAG_FORFEITS_GOLD;
        } else {
            self.flags &= !FAST_FLAG_FORFEITS_GOLD;
        }
    }

    #[inline(always)]
    pub fn set_combat_ended_turn(&mut self, v: Option<i32>) {
        self.combat_ended_turn_encoded = match v {
            Some(t) => t.clamp(0, (u16::MAX - 1) as i32) as u16,
            None => u16::MAX,
        };
    }

    #[inline]
    #[allow(clippy::too_many_arguments)]
    pub fn transposition_label(
        &self,
        score: f64,
        potion_count: u8,
        optional_paid_potion_count: u8,
        has_explicit_potion: bool,
        potion_strategic_cost: i32,
        future_sold_hp: i32,
        action_count: u16,
        forced_used_mask: u64,
    ) -> TranspositionLabel {
        TranspositionLabel::new(
            forced_used_mask,
            self.death_save_use_count as i32,
            potion_count as u16,
            potion_strategic_cost,
            future_sold_hp,
            self.cumulative_hp_lost as i32,
            action_count,
            score,
        )
        .with_details(
            has_explicit_potion,
            optional_paid_potion_count,
            self.alchemize_count,
            self.death_save_hp_restored,
        )
    }
}

/// Stage-1 fast single-pass summary scanned directly from `BattleState` before heavy Stage-2 materialization.
#[derive(Clone, Copy, Debug, Default)]
pub struct FastEvalSummary {
    pub player_hp: i32,
    pub player_max_hp: i32,
    pub player_block: i32,
    pub projected_player_hp: i32,
    pub cumulative_hp_lost: i32,
    pub recovered_hp: i32,
    pub enemy_hp: i32,
    pub raw_enemy_hp: i32,
    pub enemy_block: i32,
    pub alive_enemy_count: i32,
    pub reviving_enemy_count: i32,
    pub alive_enemy_mask: u64,
    pub player_dead: bool,
    pub all_enemies_dead: bool,
    pub won: bool,
    pub is_event_defeat: bool,
    pub has_pending_choice: bool,
    pub death_save_use_count: i32,
    pub death_save_hp_restored: i32,
    pub growth_hp_credit: i32,
    pub growth_reward_count: i32,
    pub raw_growth_reward_count: i32,
    pub growth_rewards: GrowthCounters,
    pub relic_eval: RelicCounterEvaluation,
    pub strategy_goal_hp_credit: i32,
    pub strategic_hp_credit: i32,
    pub strategy_goal_count: i32,
    pub all_strategy_targets_satisfied: bool,
    pub brightest_flame_max_hp_spent: i32,
    pub energy: i32,
    pub stars: i32,
    pub hand_count: i32,
    pub playable_hand_count: u8,
    pub live_deck_size: i32,
    pub live_deck_clutter: i32,
    pub alchemize_count: u8,
    pub empty_potion_slots: u8,
    pub feed_count: u8,
    pub outstanding_stolen: i32,
    pub forfeits_gold: bool,
    pub combat_ended_turn: Option<i32>,
    pub base_score: f64,
}

impl FastEvalSummary {
    #[inline]
    #[allow(clippy::too_many_arguments)]
    pub fn transposition_label(
        &self,
        potion_count: u16,
        optional_paid_potion_count: u8,
        has_explicit_potion: bool,
        potion_strategic_cost: i32,
        future_sold_hp: i32,
        action_count: u16,
        forced_used_mask: u64,
    ) -> TranspositionLabel {
        TranspositionLabel::new(
            forced_used_mask,
            self.death_save_use_count,
            potion_count,
            potion_strategic_cost,
            future_sold_hp,
            self.cumulative_hp_lost,
            action_count,
            self.base_score,
        )
        .with_details(
            has_explicit_potion,
            optional_paid_potion_count,
            self.alchemize_count,
            self.death_save_hp_restored.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
        )
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct StandPatEvaluation {
    pub projected_hp: i32,
    pub death_save_hp_restored: i32,
    pub all_enemies_dead: bool,
    pub resource_value: i32,
    pub delayed_damage: i32,
}

const STAND_PAT_SETS: usize = 2048;
const STAND_PAT_WAYS: usize = 4;
const STAND_PAT_SLOTS: usize = STAND_PAT_SETS * STAND_PAT_WAYS;

#[repr(C, align(64))]
struct StandPatSlot {
    seq: AtomicU32,
    _pad: u32,
    words: [AtomicU64; 5],
}

const _: () = assert!(std::mem::size_of::<StandPatSlot>() == 64);

impl StandPatSlot {
    const fn new() -> Self {
        Self {
            seq: AtomicU32::new(0),
            _pad: 0,
            words: [const { AtomicU64::new(0) }; 5],
        }
    }
}

struct StandPatStorage {
    slots: Box<[StandPatSlot]>,
    cursors: Box<[AtomicU8]>,
}

impl StandPatStorage {
    #[cold]
    #[inline(never)]
    fn alloc() -> Self {
        let slots = (0..STAND_PAT_SLOTS)
            .map(|_| StandPatSlot::new())
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let cursors = (0..STAND_PAT_SETS)
            .map(|_| AtomicU8::new(0))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self { slots, cursors }
    }
}

/// Lock-free 4-way set-associative seqlock stand-pat threat projection cache keyed by `(StateHash, round_index)`.
pub struct StandPatCache {
    storage: OnceLock<StandPatStorage>,
}

impl Default for StandPatCache {
    fn default() -> Self {
        Self::new()
    }
}

impl StandPatCache {
    pub fn new() -> Self {
        Self {
            storage: OnceLock::new(),
        }
    }

    #[inline(always)]
    fn set_index(state_hash: StateHash, round_index: i32) -> usize {
        let lo = state_hash.0 as usize;
        let hi = (state_hash.0 >> 64) as usize;
        (lo ^ hi.rotate_left(13) ^ (round_index as usize).wrapping_mul(0x9E37_79B9))
            & (STAND_PAT_SETS - 1)
    }

    #[inline]
    fn probe(&self, state_hash: StateHash, round_index: i32) -> Option<StandPatEvaluation> {
        let storage = self.storage.get()?;
        let set_idx = Self::set_index(state_hash, round_index);
        let base = set_idx * STAND_PAT_WAYS;
        let lo = state_hash.0 as u64;
        let hi = (state_hash.0 >> 64) as u64;
        let want_round = round_index as u32;

        for w in 0..STAND_PAT_WAYS {
            let slot = &storage.slots[base + w];
            let seq1 = slot.seq.load(Ordering::Acquire);
            if (seq1 & 1) != 0 || seq1 == 0 {
                continue;
            }
            let w0 = slot.words[0].load(Ordering::Relaxed);
            let w1 = slot.words[1].load(Ordering::Relaxed);
            let w2 = slot.words[2].load(Ordering::Relaxed);
            let w3 = slot.words[3].load(Ordering::Relaxed);
            let w4 = slot.words[4].load(Ordering::Relaxed);
            fence(Ordering::Acquire);
            let seq2 = slot.seq.load(Ordering::Relaxed);
            let valid = ((w4 >> 32) & 1) != 0;
            if seq1 == seq2 && valid && w0 == lo && w1 == hi && (w2 as u32) == want_round {
                return Some(StandPatEvaluation {
                    projected_hp: (w2 >> 32) as u32 as i32,
                    death_save_hp_restored: w3 as u32 as i32,
                    resource_value: (w3 >> 32) as u32 as i32,
                    delayed_damage: w4 as u32 as i32,
                    all_enemies_dead: ((w4 >> 33) & 1) != 0,
                });
            }
        }
        None
    }

    #[inline]
    fn insert(&self, state_hash: StateHash, round_index: i32, val: StandPatEvaluation) {
        let storage = self.storage.get_or_init(StandPatStorage::alloc);
        let set_idx = Self::set_index(state_hash, round_index);
        let base = set_idx * STAND_PAT_WAYS;
        let lo = state_hash.0 as u64;
        let hi = (state_hash.0 >> 64) as u64;
        let want_round = round_index as u32;

        let mut empty_way: Option<(usize, u32)> = None;
        for w in 0..STAND_PAT_WAYS {
            let slot = &storage.slots[base + w];
            let seq1 = slot.seq.load(Ordering::Acquire);
            if seq1 == 0 {
                if empty_way.is_none() {
                    empty_way = Some((w, 0));
                }
                continue;
            }
            if (seq1 & 1) != 0 {
                continue;
            }
            let w0 = slot.words[0].load(Ordering::Relaxed);
            let w1 = slot.words[1].load(Ordering::Relaxed);
            let w2 = slot.words[2].load(Ordering::Relaxed);
            let w4 = slot.words[4].load(Ordering::Relaxed);
            fence(Ordering::Acquire);
            let seq2 = slot.seq.load(Ordering::Relaxed);
            if seq1 != seq2 {
                continue;
            }
            let valid = ((w4 >> 32) & 1) != 0;
            if !valid {
                if empty_way.is_none() {
                    empty_way = Some((w, seq1));
                }
            } else if w0 == lo && w1 == hi && (w2 as u32) == want_round {
                return;
            }
        }

        let (way, expected_seq) = if let Some(pair) = empty_way {
            pair
        } else {
            let w = (storage.cursors[set_idx].fetch_add(1, Ordering::Relaxed) as usize)
                & (STAND_PAT_WAYS - 1);
            let seq = storage.slots[base + w].seq.load(Ordering::Acquire);
            if (seq & 1) != 0 {
                return;
            }
            (w, seq)
        };

        let slot = &storage.slots[base + way];
        if slot
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
        slot.words[0].store(lo, Ordering::Relaxed);
        slot.words[1].store(hi, Ordering::Relaxed);
        slot.words[2].store(
            (want_round as u64) | (((val.projected_hp as u32) as u64) << 32),
            Ordering::Relaxed,
        );
        slot.words[3].store(
            ((val.death_save_hp_restored as u32) as u64)
                | (((val.resource_value as u32) as u64) << 32),
            Ordering::Relaxed,
        );
        let flags = 1u64 | (u64::from(val.all_enemies_dead) << 1);
        slot.words[4].store(
            ((val.delayed_damage as u32) as u64) | (flags << 32),
            Ordering::Relaxed,
        );
        let next_seq = {
            let n = expected_seq.wrapping_add(2);
            if n == 0 { 2 } else { n }
        };
        slot.seq.store(next_seq, Ordering::Release);
    }

    pub fn get_or_compute(
        &self,
        state_hash: StateHash,
        round_index: i32,
        state: &BattleState,
        content: &dyn ContentDispatcher,
        scratch: &mut BattleState,
        pack_buf: &mut Vec<u8>,
    ) -> StandPatEvaluation {
        if let Some(hit) = self.probe(state_hash, round_index) {
            return hit;
        }
        let val = compute_stand_pat(state, content, scratch, pack_buf);
        self.insert(state_hash, round_index, val);
        val
    }
}

fn compute_stand_pat(
    state: &BattleState,
    content: &dyn ContentDispatcher,
    scratch: &mut BattleState,
    pack_buf: &mut Vec<u8>,
) -> StandPatEvaluation {
    if !state.is_in_progress {
        let hp = state
            .player_creature()
            .map_or(state.player.current_hp, |c| c.current_hp);
        let death_save_hp_restored = state
            .player
            .death_save_relic_hp_restored
            .saturating_add(state.player.death_save_potion_hp_restored);
        return StandPatEvaluation {
            projected_hp: hp,
            death_save_hp_restored,
            all_enemies_dead: state.won,
            resource_value: state.player.energy * 16 + state.player.stars * 8,
            delayed_damage: 0,
        };
    }

    pack_state(state, pack_buf);
    if unsafe { unpack_state(pack_buf, &state.static_ctx, scratch) }.is_err() {
        let hp = state
            .player_creature()
            .map_or(state.player.current_hp, |c| c.current_hp);
        let death_save_hp_restored = state
            .player
            .death_save_relic_hp_restored
            .saturating_add(state.player.death_save_potion_hp_restored);
        return StandPatEvaluation {
            projected_hp: hp,
            death_save_hp_restored,
            ..Default::default()
        };
    }

    let _ = CombatEngine::end_player_turn(scratch, content);
    let hp = scratch
        .player_creature()
        .map_or(scratch.player.current_hp, |c| c.current_hp);
    let death_save_hp_restored = scratch
        .player
        .death_save_relic_hp_restored
        .saturating_add(scratch.player.death_save_potion_hp_restored);
    let all_dead = scratch
        .enemy_handles
        .iter()
        .all(|&h| scratch.creature(h).is_none_or(|c| c.is_dead()));
    let mut delayed_damage = 0i32;
    for &eh in &scratch.enemy_handles {
        if let Some(c) = scratch.creature(eh) {
            if c.is_alive() {
                for p in &c.powers {
                    if matches!(p.id, PowerId::PoisonPower | PowerId::DoomPower) {
                        delayed_damage += p.amount.max(0);
                    }
                }
            }
        }
    }
    StandPatEvaluation {
        projected_hp: hp,
        death_save_hp_restored,
        all_enemies_dead: all_dead,
        resource_value: scratch.player.energy * 16 + scratch.player.stars * 8,
        delayed_damage,
    }
}

#[derive(Clone, Debug)]
pub struct RootEvalBaseline {
    pub initial_player_hp: i32,
    pub initial_player_max_hp: i32,
    pub initial_cumulative_hp_lost: i32,
    pub initial_enemy_count: usize,
    pub initial_live_deck_size: i32,
    pub start_turn_number: i32,
    pub initial_persistent_buff_value: i32,
    pub initial_retained_attack_value: i32,
    pub initial_enemy_strength_suppression: i32,
    pub initial_enemy_weak_turns: i32,
    pub encounter_boss_hp_relief: BossHpRelief,
    pub boss_hp_relief: BossHpRelief,
    pub effective_potion_policy: SolverPotionPolicy,
    pub theft_policy: SolverTheftPolicy,
    pub post_combat_relic_heal: PostCombatRelicHealProfile,
    pub is_act_ending_boss: bool,
    pub renewable_potion_shaped_rock: bool,
    pub has_gold_plated_cables: bool,
    pub has_paper_krane: bool,
    pub has_odd_mushroom: bool,
    pub has_sozu: bool,
    pub has_white_beast_statue: bool,
    pub has_on_potion_used_relic: bool,
    pub has_alchemize_in_deck: bool,
    pub belt_full_at_root: bool,
    pub empty_root_potion_slots: u8,
    pub potion_replacement_hp_credit: i32,
    pub base_replacement_credit_centi: i32,
    pub root_slot_costs: [i16; 16],
    pub paid_optional_slot_mask: u16,
    pub paid_heal_potion_slot_mask: u16,
    pub paid_fruit_juice_slot_mask: u16,
    pub paid_entropic_brew_slot_mask: u16,
    pub has_any_heal_or_unknown_generation: bool,
    pub active_relic_targets: Vec<RelicCounterTarget>,
    pub ignore_long_term_rewards: bool,
    pub effective_growth_budgets: GrowthCounters,
    pub growth_opportunity_targets: GrowthOpportunityTargets,
    pub effective_has_growth_targets: bool,
    pub mad_science_upgrade_capacity: i32,
    pub feed_max_hp_per_kill: i32,
    pub initial_growth_rewards: GrowthCounters,
    pub initial_brightest_flame_max_hp_spent: i32,
    pub root_state_brightest_flame_spent: i32,
    pub prior_battle_hp_lost: i32,
    pub all_forced_potion_mask: u64,
    pub forced_directive_count: usize,
    pub minimum_required_potion_uses: usize,
    pub has_eligible_usable_potion: bool,
    pub can_stop_at_hp_target: bool,
    pub paid_optional_potion_count: usize,
    pub min_paid_potion_cost: i32,
    pub has_paid_heal_or_growth_potion: bool,
    pub root_card_choice_quality: Vec<i16>,
    pub root_potion_slot_ids: [Option<PotionId>; 16],
    pub root_potion_slot_count: u8,
    pub root_regen_stacks: i32,
    pub has_root_potion_metadata: bool,
}

impl Default for RootEvalBaseline {
    fn default() -> Self {
        Self {
            initial_player_hp: 0,
            initial_player_max_hp: 0,
            initial_cumulative_hp_lost: 0,
            initial_enemy_count: 0,
            initial_live_deck_size: 0,
            start_turn_number: 0,
            initial_persistent_buff_value: 0,
            initial_retained_attack_value: 0,
            initial_enemy_strength_suppression: 0,
            initial_enemy_weak_turns: 0,
            encounter_boss_hp_relief: BossHpRelief::None,
            boss_hp_relief: BossHpRelief::None,
            effective_potion_policy: SolverPotionPolicy::Smart,
            theft_policy: SolverTheftPolicy::Ignore,
            post_combat_relic_heal: PostCombatRelicHealProfile::default(),
            is_act_ending_boss: false,
            renewable_potion_shaped_rock: false,
            has_gold_plated_cables: false,
            has_paper_krane: false,
            has_odd_mushroom: false,
            has_sozu: false,
            has_white_beast_statue: false,
            has_on_potion_used_relic: false,
            has_alchemize_in_deck: false,
            belt_full_at_root: false,
            empty_root_potion_slots: 0,
            potion_replacement_hp_credit: 0,
            base_replacement_credit_centi: 0,
            root_slot_costs: [-1; 16],
            paid_optional_slot_mask: 0,
            paid_heal_potion_slot_mask: 0,
            paid_fruit_juice_slot_mask: 0,
            paid_entropic_brew_slot_mask: 0,
            has_any_heal_or_unknown_generation: false,
            active_relic_targets: Vec::new(),
            ignore_long_term_rewards: false,
            effective_growth_budgets: GrowthCounters::default(),
            growth_opportunity_targets: GrowthOpportunityTargets::default(),
            effective_has_growth_targets: false,
            mad_science_upgrade_capacity: 0,
            feed_max_hp_per_kill: 3,
            initial_growth_rewards: GrowthCounters::default(),
            initial_brightest_flame_max_hp_spent: 0,
            root_state_brightest_flame_spent: 0,
            prior_battle_hp_lost: 0,
            all_forced_potion_mask: 0,
            forced_directive_count: 0,
            minimum_required_potion_uses: 0,
            has_eligible_usable_potion: false,
            can_stop_at_hp_target: false,
            paid_optional_potion_count: 0,
            min_paid_potion_cost: 0,
            has_paid_heal_or_growth_potion: false,
            root_card_choice_quality: Vec::new(),
            root_potion_slot_ids: [None; 16],
            root_potion_slot_count: 0,
            root_regen_stacks: 0,
            has_root_potion_metadata: false,
        }
    }
}

fn detect_any_heal_or_unknown_generation(
    state: &BattleState,
    content: &dyn ContentDispatcher,
) -> bool {
    for c in &state.cards {
        if c.removed_from_state || c.pile == PileType::Deck {
            continue;
        }
        if content
            .card_meta(c.id)
            .is_ok_and(|m| m.flags.contains(CardStaticFlags::MAY_HEAL))
        {
            return true;
        }
        if matches!(
            c.id,
            CardId::Abundance
                | CardId::Alchemize
                | CardId::BlotOutTheSky
                | CardId::BundleOfJoy
                | CardId::CreativeAi
                | CardId::Discovery
                | CardId::Distraction
                | CardId::Feed
                | CardId::HelloWorld
                | CardId::InfernalBlade
                | CardId::JackOfAllTrades
                | CardId::Jackpot
                | CardId::Largesse
                | CardId::MadScience
                | CardId::ManifestAuthority
                | CardId::Metamorphosis
                | CardId::NotYet
                | CardId::Quasar
                | CardId::Splash
                | CardId::Spur
                | CardId::Stoke
                | CardId::TheHierophant
                | CardId::TheLovers
                | CardId::WhiteNoise
        ) {
            return true;
        }
    }

    for cr in &state.creatures {
        for pw in &cr.powers {
            if content
                .power_meta(pw.id)
                .is_ok_and(|m| m.flags.contains(PowerStaticFlags::MAY_HEAL))
                || matches!(
                    pw.id,
                    PowerId::RegenPower | PowerId::ReattachPower | PowerId::IllusionPower
                )
            {
                return true;
            }
        }
    }

    for r in &state.player.relics {
        if r.is_melted {
            continue;
        }
        if matches!(r.id, RelicId::BurningBlood | RelicId::BlackBlood) {
            continue;
        }
        if content
            .relic_meta(r.id)
            .is_ok_and(|m| {
                m.flags
                    .intersects(RelicStaticFlags::MAY_HEAL | RelicStaticFlags::HAS_GROWTH)
            })
            || matches!(
                r.id,
                RelicId::AlchemicalCoffer
                    | RelicId::ArcaneScroll
                    | RelicId::Astrolabe
                    | RelicId::BathRobes
                    | RelicId::BathTowel
                    | RelicId::BigHat
                    | RelicId::BloodVial
                    | RelicId::BookOfFiveRings
                    | RelicId::BookRepairKnife
                    | RelicId::ChoicesParadox
                    | RelicId::Crossbow
                    | RelicId::DelicateFrond
                    | RelicId::DemonTongue
                    | RelicId::EternalFeather
                    | RelicId::FakeBloodVial
                    | RelicId::FakeLeesWaffle
                    | RelicId::HeftyTablet
                    | RelicId::LastingCandy
                    | RelicId::LeadPaperweight
                    | RelicId::LeesWaffle
                    | RelicId::LizardTail
                    | RelicId::MassiveScroll
                    | RelicId::MealTicket
                    | RelicId::MeatOnTheBone
                    | RelicId::Ocarina
                    | RelicId::OrangeDough
                    | RelicId::PandorasBox
                    | RelicId::Pantograph
                    | RelicId::PhialHolster
                    | RelicId::Planisphere
                    | RelicId::RegalPillow
                    | RelicId::SeaGlass
                    | RelicId::TeaOfDiscourtesy
                    | RelicId::Toolbox
                    | RelicId::VexingPuzzlebox
                    | RelicId::Wuppy
            )
        {
            return true;
        }
    }

    for slot in &state.player.potion_slots {
        let Some(pid) = *slot else { continue };
        if content.potion_meta(pid).is_ok_and(|m| m.may_heal)
            || matches!(
                pid,
                PotionId::Ambergris
                    | PotionId::AttackPotion
                    | PotionId::BloodPotion
                    | PotionId::BottledSong
                    | PotionId::ColorlessPotion
                    | PotionId::CosmicConcoction
                    | PotionId::DistilledChaos
                    | PotionId::EntropicBrew
                    | PotionId::FairyInABottle
                    | PotionId::FruitJuice
                    | PotionId::OrobicAcid
                    | PotionId::PowerPotion
                    | PotionId::RegenPotion
                    | PotionId::SkillPotion
                    | PotionId::SneckoOil
            )
        {
            return true;
        }
    }

    false
}

impl RootEvalBaseline {
    pub fn from_state(state: &BattleState, content: &dyn ContentDispatcher) -> Self {
        Self::from_state_with_policy(state, content, &crate::SearchPolicy::default())
    }

    pub fn from_state_with_policy(
        state: &BattleState,
        content: &dyn ContentDispatcher,
        policy: &crate::SearchPolicy,
    ) -> Self {
        let initial_player_hp = state
            .player_creature()
            .map_or(state.player.current_hp, |c| c.current_hp);
        let initial_player_max_hp = state
            .player_creature()
            .map_or(state.player.max_hp, |c| c.max_hp);
        let encounter_boss_hp_relief = if state.static_ctx.room_type == RoomType::Boss {
            if state.static_ctx.act_index < 2 {
                BossHpRelief::ActClearHeal
            } else if policy.is_non_final_act3_boss {
                BossHpRelief::None
            } else {
                BossHpRelief::RunEnding
            }
        } else {
            BossHpRelief::None
        };
        let boss_hp_relief = ActEndingBossPolicy::resolve_strategic_hp_relief(
            encounter_boss_hp_relief,
            policy.act_transition_boss_hp_strategy,
            policy.final_boss_hp_strategy,
        );
        let effective_potion_policy = if policy.theft_policy == SolverTheftPolicy::PreserveResources
        {
            SolverPotionPolicy::Smart
        } else {
            policy.potion_policy
        };
        let mut renewable_potion_shaped_rock = false;
        let mut has_gold_plated_cables = false;
        let mut has_paper_krane = false;
        let has_odd_mushroom = false;
        let mut has_sozu = false;
        let mut has_white_beast_statue = false;
        let mut has_on_potion_used_relic = false;
        for r in &state.player.relics {
            if r.is_melted {
                continue;
            }
            match r.id {
                RelicId::PetrifiedToad => renewable_potion_shaped_rock = true,
                RelicId::GoldPlatedCables => has_gold_plated_cables = true,
                RelicId::PaperKrane => has_paper_krane = true,
                RelicId::Sozu => has_sozu = true,
                RelicId::WhiteBeastStatue => has_white_beast_statue = true,
                RelicId::ReptileTrinket | RelicId::BeltBuckle => has_on_potion_used_relic = true,
                _ => {}
            }
        }
        let has_alchemize_in_deck = state
            .cards
            .iter()
            .any(|c| !c.removed_from_state && c.id == CardId::Alchemize);
        let belt_full_at_root = !state.player.potion_slots.is_empty()
            && state.player.potion_slots.iter().all(|s| s.is_some());
        let empty_root_potion_slots = state
            .player
            .potion_slots
            .iter()
            .take(16)
            .filter(|s| s.is_none())
            .count() as u8;
        let potion_replacement_hp_credit = predict_potion_replacement_hp_credit(state, policy);
        let base_replacement_credit_centi =
            potion_replacement_hp_credit.saturating_mul(weights::HP_FIXED_SCALE);

        let mut root_slot_costs = [-1i16; 16];
        let mut root_potion_slot_ids = [None; 16];
        let root_potion_slot_count = state.player.potion_slots.len().min(16) as u8;
        let root_regen_stacks = state.player_creature().map_or(0, |pc| {
            pc.powers
                .iter()
                .filter(|p| p.id == PowerId::RegenPower)
                .map(|p| p.amount)
                .sum()
        });
        let mut paid_optional_potion_count = 0usize;
        let mut paid_optional_slot_mask = 0u16;
        let mut paid_heal_potion_slot_mask = 0u16;
        let mut paid_fruit_juice_slot_mask = 0u16;
        let mut paid_entropic_brew_slot_mask = 0u16;
        let mut min_paid_potion_cost = i32::MAX;
        let mut has_paid_heal_or_growth_potion = false;
        let mut has_eligible_usable_potion = false;
        let allow_optional_at_root =
            policy.allow_potions && effective_potion_policy != SolverPotionPolicy::Disabled;
        for (slot, maybe_pid) in state.player.potion_slots.iter().enumerate().take(16) {
            root_potion_slot_ids[slot] = *maybe_pid;
            let Some(pid) = *maybe_pid else { continue };
            let cost = PotionValuationRegistry::strategic_hp_cost_for_root(
                pid,
                renewable_potion_shaped_rock,
                initial_player_max_hp,
                &policy.potion_hp_threshold_overrides,
            );
            root_slot_costs[slot] = cost.clamp(0, i16::MAX as i32) as i16;
            if pid != PotionId::FairyInABottle && policy.allows_explicit_potion_use(slot, pid, false)
            {
                has_eligible_usable_potion = true;
            }
            if pid == PotionId::FairyInABottle || !allow_optional_at_root {
                continue;
            }
            if !policy.allows_explicit_potion_use(slot, pid, false) {
                continue;
            }
            if policy.forced_bit_for(slot, pid).is_some() {
                continue;
            }
            if cost > 0 {
                let slot_bit = 1u16 << slot;
                paid_optional_potion_count += 1;
                paid_optional_slot_mask |= slot_bit;
                min_paid_potion_cost = min_paid_potion_cost.min(cost);
                let is_heal_pot = matches!(
                    pid,
                    PotionId::BloodPotion
                        | PotionId::RegenPotion
                        | PotionId::Ambergris
                        | PotionId::FruitJuice
                ) || (content.potion_meta(pid).is_ok_and(|m| m.may_heal)
                    && pid != PotionId::HeartOfIron);
                if is_heal_pot {
                    paid_heal_potion_slot_mask |= slot_bit;
                    has_paid_heal_or_growth_potion = true;
                }
                if pid == PotionId::FruitJuice {
                    paid_fruit_juice_slot_mask |= slot_bit;
                    has_paid_heal_or_growth_potion = true;
                }
                if pid == PotionId::EntropicBrew {
                    paid_entropic_brew_slot_mask |= slot_bit;
                    has_paid_heal_or_growth_potion = true;
                }
            }
        }
        if min_paid_potion_cost == i32::MAX {
            min_paid_potion_cost = 0;
        }
        if has_alchemize_in_deck && !has_sozu && allow_optional_at_root {
            has_eligible_usable_potion = true;
        }
        let has_any_heal_or_unknown_generation =
            detect_any_heal_or_unknown_generation(state, content);
        let active_relic_targets = RelicCounterPolicy::capture_targets(
            state,
            policy.relic_strategy_enabled,
            &policy.relic_counter_rules,
        );
        let growth_opportunity_targets = GrowthOpportunityTargets::capture(state, content);
        let effective_has_growth_targets =
            !policy.ignore_long_term_rewards && growth_opportunity_targets.has_targets;
        let effective_growth_budgets = if policy.ignore_long_term_rewards {
            GrowthCounters::default()
        } else {
            policy.growth_budgets
        };
        let can_stop_at_hp_target = policy.stop_at_acceptable_battle_hp_loss
            && (!effective_has_growth_targets || growth_opportunity_targets.is_bounded);
        let feed_max_hp_per_kill = if state
            .cards
            .iter()
            .any(|c| !c.removed_from_state && c.id == CardId::Feed && c.upgrade_level > 0)
        {
            4
        } else {
            3
        };
        let root_card_choice_quality = if let Some(qt) =
            content.hook_mask_tables().map(|t| t.card_generic_choice_quality)
        {
            let n = state.static_ctx.root_combat_delta_len;
            let mut v = Vec::with_capacity(n);
            for (i, c) in state.cards.iter().take(n).enumerate() {
                if !state.is_card_meta_dirty(i)
                    && c.ext_index.is_none()
                    && c.canonical_base_energy_cost >= 0
                    && c.base_energy_cost >= 0
                {
                    let cid_idx = c.id as usize;
                    if c.has_local_keyword(CardKeywords::UNPLAYABLE) {
                        v.push(-450);
                    } else if cid_idx < qt.len() {
                        let e = unsafe { qt.get_unchecked(cid_idx) };
                        if (e.flags & 1) != 0 {
                            v.push(-400);
                        } else {
                            let base_q = match c.base_energy_cost {
                                0 => e.q_cost0,
                                1 => e.q_cost1,
                                _ => e.q_cost2p,
                            };
                            let upg = if c.upgrade_level > 0 {
                                e.upg_bonus as i16
                            } else {
                                0
                            };
                            v.push(base_q + upg);
                        }
                    } else {
                        v.push(i16::MIN);
                    }
                } else {
                    v.push(i16::MIN);
                }
            }
            v
        } else {
            Vec::new()
        };

        let mut baseline = Self {
            initial_player_hp,
            initial_player_max_hp,
            initial_cumulative_hp_lost: state
                .player_creature()
                .map_or(state.player.cumulative_hp_lost, |c| c.cumulative_hp_lost),
            initial_enemy_count: state.enemy_handles.len(),
            initial_live_deck_size: (state.player.hand.len()
                + state.player.draw.len()
                + state.player.discard.len()) as i32,
            start_turn_number: state.player.turn_number,
            initial_persistent_buff_value: 0,
            initial_retained_attack_value: 0,
            initial_enemy_strength_suppression: 0,
            initial_enemy_weak_turns: 0,
            encounter_boss_hp_relief,
            boss_hp_relief,
            effective_potion_policy,
            theft_policy: policy.theft_policy,
            post_combat_relic_heal: PostCombatRelicHealProfile::from_state(state),
            is_act_ending_boss: encounter_boss_hp_relief != BossHpRelief::None,
            renewable_potion_shaped_rock,
            has_gold_plated_cables,
            has_paper_krane,
            has_odd_mushroom,
            has_sozu,
            has_white_beast_statue,
            has_on_potion_used_relic,
            has_alchemize_in_deck,
            belt_full_at_root,
            empty_root_potion_slots,
            potion_replacement_hp_credit,
            base_replacement_credit_centi,
            root_slot_costs,
            paid_optional_slot_mask,
            paid_heal_potion_slot_mask,
            paid_fruit_juice_slot_mask,
            paid_entropic_brew_slot_mask,
            has_any_heal_or_unknown_generation,
            active_relic_targets,
            ignore_long_term_rewards: policy.ignore_long_term_rewards,
            effective_growth_budgets,
            growth_opportunity_targets,
            effective_has_growth_targets,
            mad_science_upgrade_capacity: mad_science_upgrade_capacity(state, content),
            feed_max_hp_per_kill,
            initial_growth_rewards: state.semantic_history.growth_rewards,
            initial_brightest_flame_max_hp_spent: policy.initial_brightest_flame_max_hp_spent,
            root_state_brightest_flame_spent: state.semantic_history.brightest_flame_max_hp_spent,
            prior_battle_hp_lost: policy.prior_battle_hp_lost,
            all_forced_potion_mask: policy.all_forced_potion_mask(),
            forced_directive_count: policy.forced_directive_count(),
            minimum_required_potion_uses: policy.minimum_required_potion_uses(),
            has_eligible_usable_potion,
            can_stop_at_hp_target,
            paid_optional_potion_count,
            min_paid_potion_cost,
            has_paid_heal_or_growth_potion,
            root_card_choice_quality,
            root_potion_slot_ids,
            root_potion_slot_count,
            root_regen_stacks,
            has_root_potion_metadata: true,
        };
        let (_fast, full, _) =
            evaluate_fast_and_full_fused(state, content, &baseline, 0, 0, None, None);
        baseline.initial_persistent_buff_value = full.persistent_buff_value;
        baseline.initial_retained_attack_value = full.retained_attack_value;
        baseline.initial_enemy_strength_suppression = full.enemy_strength_suppression;
        baseline.initial_enemy_weak_turns = full.enemy_weak_turns;
        baseline
    }

    #[inline]
    pub fn has_active_hard_potion_constraint(&self) -> bool {
        self.all_forced_potion_mask != 0
            || (self.effective_potion_policy == SolverPotionPolicy::RequireAtLeastOne
                && self.minimum_required_potion_uses > 0)
    }

    #[inline]
    pub fn growth_target_satisfied(&self, rewards: &GrowthCounters) -> bool {
        self.can_stop_at_hp_target
            && (!self.effective_has_growth_targets
                || self.growth_opportunity_targets.is_satisfied_by(rewards))
    }

    #[inline]
    pub fn relic_targets_satisfied(&self, eval: &RelicCounterEvaluation) -> bool {
        eval.all_satisfied()
    }

    #[inline]
    pub fn brightest_flame_spent(&self, state: &BattleState) -> i32 {
        self.initial_brightest_flame_max_hp_spent
            + (state.semantic_history.brightest_flame_max_hp_spent
                - self.root_state_brightest_flame_spent)
                .max(0)
    }
}

#[inline]
pub(crate) fn is_creature_reviving(
    state: &BattleState,
    ec: &sts2_kernel::state::CreatureInst,
) -> bool {
    if !state.is_in_progress || state.won || !ec.is_dead() || state.power_extensions.is_empty() {
        return false;
    }
    if (state.hook_cache.dirty_mask & HOOK_DIRTY_CREATURES) == 0
        && state.hook_cache.cached_tables.is_some()
        && (state.hook_cache.enemy_power_flags & ENEMY_POWER_FLAG_REVIVING) == 0
    {
        #[cfg(debug_assertions)]
        debug_assert!(
            !ec.powers.iter().any(|p| {
                let key = match p.id {
                    PowerId::ReattachPower if p.amount > 0 => hot_keys::REATTACH_IS_REVIVING,
                    PowerId::AdaptablePower => hot_keys::ADAPTABLE_IS_REVIVING,
                    _ => return false,
                };
                p.ext_index.is_some_and(|ext_idx| {
                    state
                        .power_extensions
                        .get(ext_idx as usize)
                        .and_then(|pe| pe.private_fields.get_bool_static(key))
                        .unwrap_or(false)
                })
            }),
            "enemy_power_flags missed reviving enemy power!"
        );
        return false;
    }
    for p in &ec.powers {
        let key = match p.id {
            PowerId::ReattachPower if p.amount > 0 => hot_keys::REATTACH_IS_REVIVING,
            PowerId::AdaptablePower => hot_keys::ADAPTABLE_IS_REVIVING,
            _ => continue,
        };
        if let Some(ext_idx) = p.ext_index {
            if state
                .power_extensions
                .get(ext_idx as usize)
                .and_then(|pe| pe.private_fields.get_bool_static(key))
                .unwrap_or(false)
            {
                return true;
            }
        }
    }
    false
}

#[inline(always)]
fn eval_forfeits_gold(state: &BattleState) -> bool {
    state.escaped_handles.iter().any(|&eh| {
        state.creature(eh).is_some_and(|ec| {
            ec.powers
                .iter()
                .any(|p| matches!(p.id, PowerId::ThieveryPower | PowerId::SwipePower))
        })
    })
}

#[inline(always)]
fn eval_live_deck_clutter(
    state: &BattleState,
    content: &dyn ContentDispatcher,
    live_deck_size: i32,
) -> i32 {
    let mut live_deck_clutter = 0i32;
    if live_deck_size <= 0 {
        return 0;
    }
    if let Some(tables) = state.hook_cache.cached_tables {
        let sc_bits = tables.card_is_status_or_curse_bits;
        let sctx = &state.static_ctx;
        const _: () = assert!(
            PileType::Draw as u8 == 1
                && PileType::Hand as u8 == 2
                && PileType::Discard as u8 == 3
        );
        if sctx.has_root_clutter_indices && state.cards.len() >= sctx.root_initial_cards_len {
            let root_indices = if !state.deck_dirty.0 {
                &sctx.root_combat_clutter_indices[..]
            } else {
                &sctx.root_all_clutter_indices[..]
            };
            for &ci in root_indices {
                let card = unsafe { state.cards.get_unchecked(ci as usize) };
                live_deck_clutter +=
                    i32::from(!card.removed_from_state && (card.pile as u8).wrapping_sub(1) < 3);
            }
            if state.extra_clutter_hint.0 {
                if CardId::COUNT <= sc_bits.len() * 64 {
                    for card in &state.cards[sctx.root_initial_cards_len..] {
                        let active = i32::from(
                            !card.removed_from_state && (card.pile as u8).wrapping_sub(1) < 3,
                        );
                        let idx = card.id as usize;
                        let is_sc =
                            ((unsafe { *sc_bits.get_unchecked(idx >> 6) } >> (idx & 63)) & 1)
                                as i32;
                        live_deck_clutter += active & is_sc;
                    }
                } else {
                    for card in &state.cards[sctx.root_initial_cards_len..] {
                        if !card.removed_from_state && (card.pile as u8).wrapping_sub(1) < 3 {
                            let idx = card.id as usize;
                            let w = idx >> 6;
                            if w < sc_bits.len() && ((sc_bits[w] >> (idx & 63)) & 1) != 0 {
                                live_deck_clutter += 1;
                            }
                        }
                    }
                }
            }
        } else {
            let card_start = if !state.deck_dirty.0 {
                sctx.root_deck_prefix_len.min(state.cards.len())
            } else {
                0
            };
            if card_start == 0 && (live_deck_size as usize) * 2 < state.cards.len() {
                for pile in [
                    &state.player.hand[..],
                    &state.player.draw[..],
                    &state.player.discard[..],
                ] {
                    for &ch in pile {
                        if let Some(card) = state.card(ch) {
                            let idx = card.id as usize;
                            let w = idx >> 6;
                            if w < sc_bits.len() && ((sc_bits[w] >> (idx & 63)) & 1) != 0 {
                                live_deck_clutter += 1;
                            }
                        }
                    }
                }
            } else if CardId::COUNT <= sc_bits.len() * 64 {
                for card in &state.cards[card_start..] {
                    let active = i32::from(
                        !card.removed_from_state && (card.pile as u8).wrapping_sub(1) < 3,
                    );
                    let idx = card.id as usize;
                    let is_sc =
                        ((unsafe { *sc_bits.get_unchecked(idx >> 6) } >> (idx & 63)) & 1) as i32;
                    live_deck_clutter += active & is_sc;
                }
            } else {
                for card in &state.cards[card_start..] {
                    if !card.removed_from_state && (card.pile as u8).wrapping_sub(1) < 3 {
                        let idx = card.id as usize;
                        let w = idx >> 6;
                        if w < sc_bits.len() && ((sc_bits[w] >> (idx & 63)) & 1) != 0 {
                            live_deck_clutter += 1;
                        }
                    }
                }
            }
        }
        #[cfg(debug_assertions)]
        {
            let mut expected = 0i32;
            for pile in [
                &state.player.hand[..],
                &state.player.draw[..],
                &state.player.discard[..],
            ] {
                for &ch in pile {
                    if let Some(card) = state.card(ch) {
                        if tables.card_is_status_or_curse(card.id) {
                            expected += 1;
                        }
                    }
                }
            }
            debug_assert_eq!(live_deck_clutter, expected);
        }
    } else {
        for pile in [
            &state.player.hand[..],
            &state.player.draw[..],
            &state.player.discard[..],
        ] {
            for &ch in pile {
                if let Some(card) = state.card(ch) {
                    if let Ok(meta) = content.card_meta(card.id) {
                        if matches!(meta.card_type, CardType::Status | CardType::Curse) {
                            live_deck_clutter += 1;
                        }
                    }
                }
            }
        }
    }
    live_deck_clutter
}

#[derive(Default)]
struct EnemyFastEvalAcc {
    enemy_hp: i32,
    raw_enemy_hp: i32,
    enemy_block: i32,
    alive_enemy_count: i32,
    reviving_enemy_count: i32,
    alive_enemy_mask: u64,
    outstanding_stolen: i32,
}

#[derive(Default)]
struct EnemyFullEvalAcc {
    enemy_debuff_stacks: i32,
    enemy_debuff_value: i32,
    enemy_threat_power_penalty: i32,
    delayed_damage_value: i32,
    enemy_strength_suppression: i32,
    enemy_weak_turns: i32,
    enemy_vulnerable_turns: i32,
    min_sandpit_turns: i32,
    reviving_hp_penalty: i32,
    reattach_alive_count: i32,
    total_poison_and_doom: i32,
    doom_lethal_count: i32,
    poison_lethal_count: i32,
    poison_lethal_hp_offset: i32,
    effective_doom_value: i32,
    poison_hp_normal: i64,
    poison_hp_reviving: i64,
    alive_non_minion_enemies: i32,
    enemy_attack_damage: i32,
    incoming_damage: i32,
    unblocked_incoming: i32,
    paper_cuts_max_hp_threat: i32,
    enrage_phase_threat_dmg: i32,
    remaining_block: i32,
    remaining_osty: i32,
    remaining_wuppy: i32,
    remaining_buffer: i32,
    osty_absorbed: i32,
    highest_alive_enemy_hp: i32,
    focus_target_remaining_hp: i32,
}

impl EnemyFullEvalAcc {
    #[inline(always)]
    fn from_pre_enemy(pre: &PlayerPreEnemyState) -> Self {
        Self {
            min_sandpit_turns: i32::MAX,
            focus_target_remaining_hp: i32::MAX,
            incoming_damage: pre.self_incoming_damage,
            unblocked_incoming: pre.self_unblocked_incoming,
            remaining_block: pre.remaining_block,
            remaining_osty: pre.osty_hp,
            remaining_wuppy: pre.wuppy_hp,
            remaining_buffer: pre.remaining_buffer,
            ..Default::default()
        }
    }
}

#[derive(Default)]
struct PlayerPowerEvalAcc {
    persistent_buff_value: i32,
    expected_regen_heal: i32,
    player_buff_stacks: i32,
    future_resource_value: i32,
    player_focus: i32,
    player_vuln: i32,
    player_intangible: i32,
    player_buffer: i32,
    player_disintegration: i32,
    player_constrict: i32,
    player_demise: i32,
    player_accelerant: i32,
    player_end_turn_block: i32,
    player_retain_hand: i32,
    has_player_corruption: bool,
    has_blade_synergy: bool,
    has_player_hex: bool,
    no_draw_setup_penalty: i32,
    preserves_excess_block: bool,
    next_turn_block: i32,
    next_turn_energy: i32,
    next_turn_draw: i32,
    next_turn_stars: i32,
    next_turn_summon: i32,
    furnace_power: i32,
    rewarded_the_hunts: i32,
}

#[derive(Default)]
struct OrbPreEvalAcc {
    orb_capacity: i32,
    orb_score: i32,
    dark_evoke_sum: i32,
    max_dark_evoke: i32,
}

#[derive(Default)]
struct PlayerPreEnemyState {
    p_acc: PlayerPowerEvalAcc,
    orb_acc: OrbPreEvalAcc,
    osty_hp: i32,
    osty_max_hp: i32,
    wuppy_hp: i32,
    end_turn_block: i32,
    pet_defense_buffer: i32,
    remaining_block: i32,
    remaining_buffer: i32,
    self_incoming_damage: i32,
    self_unblocked_incoming: i32,
}

#[inline(always)]
const fn is_temporary_enemy_strength_down_power(power_id: PowerId) -> bool {
    matches!(
        power_id,
        PowerId::ShacklingPotionPower
            | PowerId::PiercingWailPower
            | PowerId::DarkShacklesPower
            | PowerId::EnfeeblingTouchPower
            | PowerId::CrushUnderPower
            | PowerId::DyingStarPower
            | PowerId::ManglePower
            | PowerId::MonarchsGazeStrengthDownPower
    )
}

#[inline(always)]
fn apply_self_end_turn_hit(
    raw_damage: i32,
    blockable: bool,
    player_intangible: bool,
    remaining_block: &mut i32,
    remaining_buffer: &mut i32,
    incoming_damage: &mut i32,
    unblocked_incoming: &mut i32,
) {
    if raw_damage <= 0 {
        return;
    }
    let mut dmg = if player_intangible && raw_damage > 1 {
        1
    } else {
        raw_damage
    };
    if blockable {
        *incoming_damage += dmg;
        if *remaining_block > 0 {
            let blocked = (*remaining_block).min(dmg);
            *remaining_block -= blocked;
            dmg -= blocked;
        }
    }
    if dmg > 0 {
        if *remaining_buffer > 0 {
            *remaining_buffer -= 1;
        } else {
            *unblocked_incoming += dmg;
        }
    }
}

#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn apply_single_enemy_hit(
    hit: i32,
    enemy_paper_cuts: i32,
    remaining_block: &mut i32,
    remaining_osty: &mut i32,
    remaining_wuppy: &mut i32,
    remaining_buffer: &mut i32,
    osty_absorbed: &mut i32,
    unblocked_damage: &mut i32,
    paper_cuts_max_hp_loss: &mut i32,
) {
    let mut rem = hit;
    if *remaining_osty > 0 && rem > 0 {
        let absorbed = (*remaining_osty).min(rem);
        *remaining_osty -= absorbed;
        *osty_absorbed += absorbed;
        rem -= absorbed;
    }
    if *remaining_wuppy > 0 && rem > 0 {
        let absorbed = (*remaining_wuppy).min(rem);
        *remaining_wuppy -= absorbed;
        rem -= absorbed;
    }
    if *remaining_block > 0 && rem > 0 {
        let blocked = (*remaining_block).min(rem);
        *remaining_block -= blocked;
        rem -= blocked;
    }
    if rem > 0 {
        if *remaining_buffer > 0 {
            *remaining_buffer -= 1;
        } else {
            *unblocked_damage += rem;
            if enemy_paper_cuts > 0 {
                *paper_cuts_max_hp_loss += enemy_paper_cuts;
            }
        }
    }
}

#[inline(always)]
fn scale_enemy_hit(
    base_damage: i32,
    enemy_strength: i32,
    enemy_weak: bool,
    player_vulnerable: bool,
    player_intangible: bool,
    root: &RootEvalBaseline,
) -> i32 {
    let mut dmg = base_damage + enemy_strength;
    if dmg <= 0 {
        return 0;
    }
    if enemy_weak {
        dmg = if root.has_paper_krane {
            (dmg * 60) / 100
        } else {
            let res = (dmg * 3) >> 2;
            #[cfg(debug_assertions)]
            debug_assert_eq!(res, (dmg * 75) / 100);
            res
        };
    }
    if player_vulnerable {
        dmg = if root.has_odd_mushroom {
            let res = (dmg * 5) >> 2;
            #[cfg(debug_assertions)]
            debug_assert_eq!(res, (dmg * 125) / 100);
            res
        } else {
            let res = (dmg * 3) >> 1;
            #[cfg(debug_assertions)]
            debug_assert_eq!(res, (dmg * 150) / 100);
            res
        };
    }
    if player_intangible && dmg > 1 {
        dmg = 1;
    }
    dmg.max(0)
}

#[allow(clippy::too_many_arguments)]
fn estimate_activated_phase_threat(
    state: &BattleState,
    ec: &CreatureInst,
    enemy_strength: i32,
    enemy_weak_next: bool,
    player_vuln_next: bool,
    player_intangible_next: bool,
    alive_enemies: i32,
    root: &RootEvalBaseline,
) -> i32 {
    let Some(m) = ec.monster.as_deref() else {
        return 0;
    };
    let cur_str = m
        .next_move_id
        .as_deref()
        .unwrap_or(m.current_state_id.as_ref());
    let deadly = |high: i32, low: i32| {
        if is_ascension_active_in_mode(
            state.static_ctx.game_mode,
            state.static_ctx.ascension,
            AscensionLevel::DeadlyEnemies,
        ) {
            high
        } else {
            low
        }
    };

    let (base_dmg, hits, bonus_str) = match m.monster_id {
        MonsterId::Queen => {
            let amalgam_died = matches!(
                m.private_fields.get("Queen._hasAmalgamDied"),
                Some(FieldValue::Bool(true))
            );
            if amalgam_died
                && matches!(
                    cur_str,
                    "YOU_ARE_MINE_MOVE"
                        | "DREAD_INITIAL_MOVE"
                        | "BURN_BRIGHT_FOR_ME_MOVE"
                        | "YOURE_MINE_NOW_BRANCH"
                        | "BURN_BRIGHT_FOR_ME_BRANCH"
                        | "ENRAGE_MOVE"
                )
            {
                let bonus = if cur_str == "ENRAGE_MOVE" { 2 } else { 0 };
                (deadly(4, 3), 5, bonus)
            } else {
                return 0;
            }
        }
        MonsterId::LivingShield => {
            if alive_enemies <= 1
                && matches!(cur_str, "SHIELD_SLAM_MOVE" | "SHIELD_SLAM_BRANCH")
            {
                (deadly(18, 16), 1, 0)
            } else {
                return 0;
            }
        }
        MonsterId::FrogKnight => {
            let charged = matches!(
                m.private_fields.get("FrogKnight._charged"),
                Some(FieldValue::Bool(true))
            );
            if !charged
                && ec.current_hp < ec.max_hp / 2
                && matches!(cur_str, "FOR_THE_QUEEN" | "HALF_HEALTH")
            {
                let bonus = if cur_str == "FOR_THE_QUEEN" { 5 } else { 0 };
                (deadly(40, 35), 1, bonus)
            } else {
                return 0;
            }
        }
        _ => return 0,
    };

    let per_hit = scale_enemy_hit(
        base_dmg,
        enemy_strength + bonus_str,
        enemy_weak_next,
        player_vuln_next,
        player_intangible_next,
        root,
    );
    per_hit.max(0) * hits
}

#[inline(always)]
fn card_effective_keywords(card: &CardInst) -> CardKeywords {
    if card.keywords_added_from_outside.is_empty() && card.keywords_removed_from_outside.is_empty() {
        card.local_keywords
    } else {
        card.effective_keywords_base()
    }
}

#[inline(always)]
fn card_is_playable_in_hand(
    state: &BattleState,
    card: &CardInst,
    meta: &CardMeta,
    has_player_corruption: bool,
) -> (bool, i32, i32, bool, bool) {
    let kw = card_effective_keywords(card);
    if kw.contains(CardKeywords::UNPLAYABLE) || card.canonical_base_energy_cost < -1 {
        return (false, 0, 0, false, false);
    }
    let is_costs_x = meta.flags.contains(CardStaticFlags::IS_X_ENERGY_COST);
    let is_star_costs_x = meta.flags.contains(CardStaticFlags::IS_X_STAR_COST);
    let mut live_cost = if is_costs_x {
        state.player.energy.max(0)
    } else if card.canonical_base_energy_cost < 0 {
        i32::MAX
    } else if card.ext_index.is_none() {
        (card.base_energy_cost as i32).max(0)
    } else {
        sts2_kernel::ordering::resolve_local_energy_cost_card(state, card, false).max(0)
    };
    if has_player_corruption && meta.card_type == CardType::Skill && !is_costs_x {
        live_cost = 0;
    }
    let star_cost = if is_star_costs_x {
        state.player.stars.max(0)
    } else if card.temp_star_cost_override_this_turn.is_none() && card.temp_flags.is_empty() {
        meta.canonical_star_cost.max(0)
    } else {
        sts2_kernel::ordering::resolve_local_star_cost_card(card, meta.canonical_star_cost).max(0)
    };
    let playable = live_cost <= state.player.energy && star_cost <= state.player.stars;
    (playable, live_cost, star_cost, is_costs_x, is_star_costs_x)
}

#[inline]
fn evaluate_nightmare_bonus(
    state: &BattleState,
    content: &dyn ContentDispatcher,
    power: &sts2_kernel::state::PowerInst,
    card_metas: Option<&[CardMeta]>,
    card_rollout_profiles: Option<&[CardRolloutProfile]>,
) -> Option<i32> {
    let ext_idx = power.ext_index?;
    let pe = state.power_extensions.get(ext_idx as usize)?;
    let selected_handle = match pe.private_fields.get("NightmarePower._selectedCard")? {
        FieldValue::CardHandle(h) if *h > 0 => CardHandle(*h as u32),
        _ => return None,
    };
    let template = state.card(selected_handle)?;
    let meta = match card_metas.and_then(|cm| cm.get(template.id as usize)) {
        Some(m) => m,
        None => content.card_meta(template.id).ok()?,
    };
    if matches!(meta.card_type, CardType::Status | CardType::Curse) {
        return Some(-18);
    }
    let prof = card_rollout_profiles.and_then(|t| t.get(template.id as usize));
    let mut card_val = prof.map_or(0, |pr| i32::from(pr.setup_weight));
    if meta.card_type == CardType::Power {
        card_val += 16;
    } else if meta.rarity == CardRarity::Rare {
        card_val += 10;
    } else if meta.rarity == CardRarity::Uncommon {
        card_val += 4;
    } else if meta.rarity == CardRarity::Basic {
        card_val -= 8;
    }
    if template.upgrade_level > 0 {
        card_val += 5;
    }
    if meta.flags.contains(CardStaticFlags::GAINS_BLOCK)
        || prof.is_some_and(|pr| pr.base_block > 0 || pr.upg_block > 0)
    {
        card_val += 5;
    }
    if meta.flags.contains(CardStaticFlags::MAY_HEAL) || template.id == CardId::Alchemize {
        card_val += 10;
    }
    let cost = if meta.flags.contains(CardStaticFlags::IS_X_ENERGY_COST) {
        2
    } else {
        sts2_kernel::ordering::resolve_local_energy_cost_card(state, template, false).max(0)
    };
    if cost == 0 && meta.rarity != CardRarity::Basic {
        card_val += 8;
    } else if cost >= 3 && meta.card_type != CardType::Power {
        card_val -= 4;
    }
    let copies = power.amount.clamp(1, 3);
    Some(((card_val * copies) / 3).clamp(2, 80))
}

#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn eval_player_powers(
    state: &BattleState,
    pc: Option<&CreatureInst>,
    content: &dyn ContentDispatcher,
    power_metas: Option<&[PowerMeta]>,
    card_metas: Option<&[CardMeta]>,
    card_rollout_profiles: Option<&[CardRolloutProfile]>,
    regen_cap: i32,
    track_the_hunt: bool,
) -> PlayerPowerEvalAcc {
    let mut acc = PlayerPowerEvalAcc::default();
    if let Some(pc) = pc {
        for p in &pc.powers {
            if p.amount == 0 {
                continue;
            }
            let amt = p.amount.max(0);
            let mut is_one_turn_carryover = false;
            match p.id {
                PowerId::BlockNextTurnPower
                | PowerId::SelfFormingClayPower
                | PowerId::ToricToughnessPower => {
                    is_one_turn_carryover = true;
                    acc.next_turn_block += amt;
                    acc.future_resource_value += amt * 2;
                }
                PowerId::EnergyNextTurnPower => {
                    is_one_turn_carryover = true;
                    acc.next_turn_energy += amt;
                    acc.future_resource_value += amt * 3;
                }
                PowerId::DrawCardsNextTurnPower => {
                    is_one_turn_carryover = true;
                    acc.next_turn_draw += amt;
                    acc.future_resource_value += amt * 2;
                }
                PowerId::StarNextTurnPower => {
                    is_one_turn_carryover = true;
                    acc.next_turn_stars += amt;
                }
                PowerId::SummonNextTurnPower => {
                    is_one_turn_carryover = true;
                    acc.next_turn_summon += amt;
                }
                PowerId::FurnacePower => acc.furnace_power += p.amount,
                PowerId::VulnerablePower => acc.player_vuln += amt,
                PowerId::IntangiblePower => acc.player_intangible += amt,
                PowerId::BufferPower if p.amount > 0 => acc.player_buffer += p.amount,
                PowerId::DisintegrationPower if p.amount > 0 => acc.player_disintegration += p.amount,
                PowerId::ConstrictPower if p.amount > 0 => acc.player_constrict += p.amount,
                PowerId::DemisePower if p.amount > 0 => acc.player_demise += p.amount,
                PowerId::AccelerantPower => acc.player_accelerant += amt,
                PowerId::RetainHandPower | PowerId::WellLaidPlansPower if p.amount > 0 => {
                    acc.player_retain_hand += p.amount;
                }
                PowerId::CorruptionPower if p.amount > 0 => acc.has_player_corruption = true,
                PowerId::HexPower if p.amount > 0 => acc.has_player_hex = true,
                PowerId::SeekingEdgePower | PowerId::ParryPower if p.amount > 0 => {
                    acc.has_blade_synergy = true;
                }
                PowerId::TheHuntPower if track_the_hunt && p.amount > 0 => {
                    acc.rewarded_the_hunts += p.amount;
                }
                PowerId::BarricadePower | PowerId::BlurPower => acc.preserves_excess_block = true,
                PowerId::PlatingPower => acc.player_end_turn_block += amt,
                PowerId::FocusPower => acc.player_focus += p.amount,
                PowerId::RegenPower if p.amount > 0 => {
                    let capped_stacks = amt.min(10);
                    let tri = (capped_stacks * (capped_stacks + 1) / 2).min(25);
                    acc.expected_regen_heal += tri.min(regen_cap);
                }
                _ => {}
            }
            let meta = match power_metas.and_then(|pm| pm.get(p.id as usize)) {
                Some(pm) => Some(pm),
                None => content.power_meta(p.id).ok(),
            };
            if !is_one_turn_carryover
                && p.amount > 0
                && meta.is_some_and(|m| m.power_type == PowerType::Buff)
            {
                acc.player_buff_stacks += 6 + p.amount.min(10_000);
            }
            if p.id == PowerId::NightmarePower && p.amount > 0 {
                if let Some(nightmare_w) =
                    evaluate_nightmare_bonus(state, content, p, card_metas, card_rollout_profiles)
                {
                    acc.persistent_buff_value += nightmare_w;
                    continue;
                }
            }
            let power_setup_weight = compute_player_power_setup_weight(p.id, p.amount, meta);
            acc.persistent_buff_value += power_setup_weight;
            if p.id == PowerId::NoDrawPower && power_setup_weight < 0 {
                acc.no_draw_setup_penalty += power_setup_weight;
            }
        }
    }
    acc
}

#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn eval_player_pre_enemy_state(
    state: &BattleState,
    content: &dyn ContentDispatcher,
    root: &RootEvalBaseline,
    power_metas: Option<&[PowerMeta]>,
    card_metas: Option<&[CardMeta]>,
    card_rollout_profiles: Option<&[CardRolloutProfile]>,
    player_hp: i32,
    player_max_hp: i32,
    player_block: i32,
) -> PlayerPreEnemyState {
    let regen_cap = (player_max_hp - player_hp).max(0) + 12;
    let track_the_hunt = !root.ignore_long_term_rewards && root.effective_has_growth_targets;
    let p_acc = eval_player_powers(
        state,
        state.player_creature(),
        content,
        power_metas,
        card_metas,
        card_rollout_profiles,
        regen_cap,
        track_the_hunt,
    );

    let has_cables = root.has_gold_plated_cables;
    let orb_capacity = state.player.orb_capacity.clamp(0, 10);
    let mut orb_score = 0i32;
    let mut frost_passive_block = 0i32;
    let mut dark_evoke_sum = 0i32;
    let mut max_dark_evoke = 0i32;
    for (o_idx, orb) in state.player.orbs.iter().enumerate() {
        let val = match orb.id {
            OrbId::LightningOrb => {
                let passive = (orb
                    .raw_passive_value
                    .and_then(|d| d.to_i32_trunc().ok())
                    .unwrap_or(3)
                    + p_acc.player_focus)
                    .max(0);
                let evoke = (orb
                    .raw_evoke_value
                    .and_then(|d| d.to_i32_trunc().ok())
                    .unwrap_or(8)
                    + p_acc.player_focus)
                    .max(0);
                passive * 2 + evoke
            }
            OrbId::FrostOrb => {
                let passive = (orb
                    .raw_passive_value
                    .and_then(|d| d.to_i32_trunc().ok())
                    .unwrap_or(2)
                    + p_acc.player_focus)
                    .max(0);
                let evoke = (orb
                    .raw_evoke_value
                    .and_then(|d| d.to_i32_trunc().ok())
                    .unwrap_or(5)
                    + p_acc.player_focus)
                    .max(0);
                let mult = if o_idx == 0 && has_cables { 2 } else { 1 };
                frost_passive_block += passive * mult;
                passive * 3 + evoke
            }
            OrbId::DarkOrb => {
                let passive = (orb
                    .raw_passive_value
                    .and_then(|d| d.to_i32_trunc().ok())
                    .unwrap_or(6)
                    + p_acc.player_focus)
                    .max(0);
                let evoke = orb
                    .raw_evoke_value
                    .and_then(|d| d.to_i32_trunc().ok())
                    .unwrap_or(6)
                    .max(0);
                dark_evoke_sum += evoke;
                max_dark_evoke = max_dark_evoke.max(evoke);
                evoke + passive * 2
            }
            OrbId::PlasmaOrb => 18,
            OrbId::GlassOrb => {
                let passive = (orb
                    .raw_passive_value
                    .and_then(|d| d.to_i32_trunc().ok())
                    .unwrap_or(4)
                    + p_acc.player_focus)
                    .max(0);
                let evoke = (orb
                    .raw_evoke_value
                    .and_then(|d| d.to_i32_trunc().ok())
                    .unwrap_or(8)
                    + p_acc.player_focus)
                    .max(0);
                passive * 2 + evoke
            }
        };
        orb_score += val;
    }

    let (osty_hp, osty_max_hp, wuppy_hp) = if state.pet_handles.is_empty() {
        (0, 0, 0)
    } else {
        let (o_hp, o_max) = state
            .osty_handle()
            .and_then(|h| state.creature(h))
            .filter(|c| c.is_alive())
            .map_or((0, 0), |c| (c.current_hp.max(0), c.max_hp.max(0)));
        let w_hp: i32 = state
            .pet_handles
            .iter()
            .filter_map(|&h| state.creature(h))
            .filter(|c| c.is_alive() && c.monster_id == Some(MonsterId::Wuppy))
            .map(|c| c.current_hp.max(0))
            .sum();
        (o_hp, o_max, w_hp)
    };

    let end_turn_block = player_block + p_acc.player_end_turn_block + frost_passive_block;
    let pet_defense_buffer = wuppy_hp + osty_hp;
    let mut remaining_block = end_turn_block;
    let mut remaining_buffer = p_acc.player_buffer;
    let mut self_incoming_damage = 0i32;
    let mut self_unblocked_incoming = 0i32;
    let intangible = p_acc.player_intangible > 0;

    for &ch in &state.player.hand {
        if let Some(card) = state.card(ch) {
            match card.id {
                CardId::Burn => {
                    let dmg = if card.upgrade_level > 0 { 4 } else { 2 };
                    apply_self_end_turn_hit(
                        dmg,
                        true,
                        intangible,
                        &mut remaining_block,
                        &mut remaining_buffer,
                        &mut self_incoming_damage,
                        &mut self_unblocked_incoming,
                    );
                }
                CardId::Decay => {
                    apply_self_end_turn_hit(
                        2,
                        true,
                        intangible,
                        &mut remaining_block,
                        &mut remaining_buffer,
                        &mut self_incoming_damage,
                        &mut self_unblocked_incoming,
                    );
                }
                CardId::Beckon => {
                    apply_self_end_turn_hit(
                        6,
                        false,
                        intangible,
                        &mut remaining_block,
                        &mut remaining_buffer,
                        &mut self_incoming_damage,
                        &mut self_unblocked_incoming,
                    );
                }
                _ => {}
            }
        }
    }
    if p_acc.player_disintegration > 0 {
        apply_self_end_turn_hit(
            p_acc.player_disintegration,
            true,
            intangible,
            &mut remaining_block,
            &mut remaining_buffer,
            &mut self_incoming_damage,
            &mut self_unblocked_incoming,
        );
    }
    if p_acc.player_constrict > 0 {
        apply_self_end_turn_hit(
            p_acc.player_constrict,
            true,
            intangible,
            &mut remaining_block,
            &mut remaining_buffer,
            &mut self_incoming_damage,
            &mut self_unblocked_incoming,
        );
    }
    if p_acc.player_demise > 0 {
        apply_self_end_turn_hit(
            p_acc.player_demise,
            false,
            intangible,
            &mut remaining_block,
            &mut remaining_buffer,
            &mut self_incoming_damage,
            &mut self_unblocked_incoming,
        );
    }

    PlayerPreEnemyState {
        p_acc,
        orb_acc: OrbPreEvalAcc {
            orb_capacity,
            orb_score,
            dark_evoke_sum,
            max_dark_evoke,
        },
        osty_hp,
        osty_max_hp,
        wuppy_hp,
        end_turn_block,
        pet_defense_buffer,
        remaining_block,
        remaining_buffer,
        self_incoming_damage,
        self_unblocked_incoming,
    }
}

#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn eval_enemies_pass<const WANT_FAST: bool, const WANT_FULL: bool>(
    state: &BattleState,
    content: &dyn ContentDispatcher,
    root: &RootEvalBaseline,
    power_metas: Option<&[PowerMeta]>,
    player_accelerant: i32,
    player_vuln: i32,
    player_intangible: i32,
    mut intent_cache: Option<&mut IntentHitsCache>,
    fast_acc: &mut EnemyFastEvalAcc,
    full_acc: &mut EnemyFullEvalAcc,
) {
    let check_gold_theft = WANT_FAST
        && !((state.hook_cache.dirty_mask & HOOK_DIRTY_CREATURES) == 0
            && state.hook_cache.cached_tables.is_some()
            && (state.hook_cache.enemy_power_flags & ENEMY_POWER_FLAG_GOLD_THEFT) == 0);

    let clamped_accelerant = player_accelerant.clamp(0, 16);
    let alive_enemy_total_for_phase = if WANT_FULL {
        state
            .enemy_handles
            .iter()
            .filter(|&&h| state.creature(h).is_some_and(|c| c.is_alive()))
            .count() as i32
    } else {
        0
    };

    for (idx, &eh) in state.enemy_handles.iter().enumerate() {
        let Some(ec) = state.creature(eh) else {
            continue;
        };
        #[cfg(debug_assertions)]
        debug_assert!(state.contains_creature(eh));
        let hp = ec.current_hp.max(0);
        if WANT_FAST {
            fast_acc.raw_enemy_hp += hp;
        }
        if ec.is_alive() {
            if WANT_FAST {
                fast_acc.enemy_hp += hp;
                fast_acc.enemy_block += ec.block.max(0);
                fast_acc.alive_enemy_count += 1;
                if idx < 64 {
                    fast_acc.alive_enemy_mask |= 1u64 << idx;
                }
            }
            if WANT_FULL && hp > full_acc.highest_alive_enemy_hp {
                full_acc.highest_alive_enemy_hp = hp;
            }

            if !WANT_FULL {
                if check_gold_theft && !ec.powers.is_empty() {
                    for p in &ec.powers {
                        if p.id == PowerId::ThieveryPower {
                            if let Some(ext_idx) = p.ext_index {
                                if let Some(FieldValue::Int(gold)) = state
                                    .power_extensions
                                    .get(ext_idx as usize)
                                    .and_then(|pe| pe.private_fields.get("ThieveryPower._goldStolen"))
                                {
                                    fast_acc.outstanding_stolen += (*gold).max(0);
                                }
                            }
                        } else if p.id == PowerId::SwipePower && p.amount > 0 {
                            fast_acc.outstanding_stolen += p.amount;
                        }
                    }
                } else {
                    #[cfg(debug_assertions)]
                    if WANT_FAST {
                        debug_assert!(
                            !ec.powers.iter().any(|p| {
                                (p.id == PowerId::ThieveryPower && p.ext_index.is_some())
                                    || (p.id == PowerId::SwipePower && p.amount > 0)
                            }),
                            "enemy_power_flags missed ThieveryPower/SwipePower on {:?}",
                            eh
                        );
                    }
                }
            } else {
                let mut enemy_str = 0i32;
                let mut enemy_temp_str_recovery = 0i32;
                let mut enemy_vigor = 0i32;
                let mut enemy_paper_cuts = 0i32;
                let mut enemy_weak = 0i32;
                let mut enemy_poison = 0i32;
                let mut enemy_doom = 0i32;
                let mut has_reattach = false;
                let mut can_revive = false;
                let mut has_damage_cap = false;
                let mut is_minion = ec.has_minion_power;

                if WANT_FAST && !check_gold_theft {
                    #[cfg(debug_assertions)]
                    debug_assert!(
                        !ec.powers.iter().any(|p| {
                            (p.id == PowerId::ThieveryPower && p.ext_index.is_some())
                                || (p.id == PowerId::SwipePower && p.amount > 0)
                        }),
                        "enemy_power_flags missed ThieveryPower/SwipePower on {:?}",
                        eh
                    );
                }

                for p in &ec.powers {
                    if WANT_FAST && check_gold_theft {
                        if p.id == PowerId::ThieveryPower {
                            if let Some(ext_idx) = p.ext_index {
                                if let Some(FieldValue::Int(gold)) = state
                                    .power_extensions
                                    .get(ext_idx as usize)
                                    .and_then(|pe| pe.private_fields.get("ThieveryPower._goldStolen"))
                                {
                                    fast_acc.outstanding_stolen += (*gold).max(0);
                                }
                            }
                        } else if p.id == PowerId::SwipePower && p.amount > 0 {
                            fast_acc.outstanding_stolen += p.amount;
                        }
                    }
                    if p.amount == 0 {
                        continue;
                    }
                    if p.id == PowerId::ReattachPower && p.amount > 0 {
                        has_reattach = true;
                        continue;
                    }
                    if p.id == PowerId::MinionPower && p.amount > 0 {
                        is_minion = true;
                    }

                    let is_temp_str_down = is_temporary_enemy_strength_down_power(p.id);
                    if p.amount > 0 && !is_temp_str_down {
                        let pm_opt = match power_metas.and_then(|pm| pm.get(p.id as usize)) {
                            Some(pm) => Some(pm),
                            None => content.power_meta(p.id).ok(),
                        };
                        if let Some(pm) = pm_opt {
                            if matches!(pm.power_type, PowerType::Debuff) {
                                let is_dot =
                                    matches!(p.id, PowerId::PoisonPower | PowerId::DoomPower);
                                full_acc.enemy_debuff_stacks += if is_dot {
                                    p.amount.min(10_000)
                                } else {
                                    p.amount.min(4)
                                };
                            }
                        }
                    }

                    if is_temp_str_down && p.amount > 0 {
                        enemy_temp_str_recovery += p.amount;
                    } else {
                        match p.id {
                            PowerId::StrengthPower => {
                                enemy_str += p.amount;
                            }
                            PowerId::VigorPower if p.amount > 0 => {
                                enemy_vigor += p.amount;
                            }
                            PowerId::PaperCutsPower if p.amount > 0 => {
                                enemy_paper_cuts += p.amount;
                            }
                            PowerId::WeakPower if p.amount > 0 => {
                                enemy_weak += p.amount;
                                full_acc.enemy_weak_turns += p.amount;
                                full_acc.enemy_debuff_value += p.amount.min(5) * 4;
                            }
                            PowerId::VulnerablePower if p.amount > 0 => {
                                full_acc.enemy_vulnerable_turns += p.amount;
                                full_acc.enemy_debuff_value += p.amount.min(5) * 5;
                            }
                            PowerId::PoisonPower if p.amount > 0 => {
                                let amt = p.amount;
                                enemy_poison += amt;
                                full_acc.delayed_damage_value += amt;
                            }
                            PowerId::DoomPower if p.amount > 0 => {
                                let amt = p.amount;
                                enemy_doom += amt;
                                let d = amt.min(hp.max(1));
                                full_acc.delayed_damage_value +=
                                    ((d as i64 * d as i64) / hp.max(1) as i64) as i32;
                            }
                            PowerId::RitualPower if p.amount > 0 => {
                                full_acc.enemy_threat_power_penalty += p.amount.min(10) * 5;
                            }
                            PowerId::SandpitPower if p.amount > 0 => {
                                full_acc.min_sandpit_turns =
                                    full_acc.min_sandpit_turns.min(p.amount);
                            }
                            PowerId::IntangiblePower
                            | PowerId::BufferPower
                            | PowerId::SlipperyPower
                            | PowerId::HardToKillPower
                            | PowerId::HardenedShellPower
                                if p.amount > 0 =>
                            {
                                has_damage_cap = true;
                            }
                            PowerId::AdaptablePower | PowerId::IllusionPower
                                if p.amount > 0 =>
                            {
                                can_revive = true;
                            }
                            _ => {}
                        }
                    }
                }

                let is_reviving_enemy = has_reattach || can_revive;
                if has_reattach {
                    full_acc.reattach_alive_count += 1;
                }
                full_acc.total_poison_and_doom +=
                    enemy_poison.min(100_000) + enemy_doom.min(100_000);

                let ticks = enemy_poison.min(1 + clamped_accelerant);
                let poison_tick_damage = if ticks > 0 {
                    (i64::from(ticks) * (2 * i64::from(enemy_poison) - i64::from(ticks) + 1)) / 2
                } else {
                    0
                };
                let poison_lethal_before_attack = hp > 0
                    && poison_tick_damage >= i64::from(hp)
                    && !is_reviving_enemy
                    && !has_damage_cap;
                if poison_lethal_before_attack {
                    full_acc.poison_lethal_count += 1;
                    full_acc.poison_lethal_hp_offset += hp;
                    full_acc.focus_target_remaining_hp = 0;
                    continue;
                }

                let mut doom_lethal = false;
                if enemy_doom > 0 {
                    if enemy_doom >= hp && !is_reviving_enemy {
                        doom_lethal = true;
                        full_acc.doom_lethal_count += 1;
                        full_acc.effective_doom_value += hp;
                    } else {
                        full_acc.effective_doom_value += enemy_doom.min(hp);
                    }
                }

                if enemy_poison > 0 && hp > 0 && !doom_lethal {
                    let expected_ticks = enemy_poison.min(2 + clamped_accelerant);
                    let expected_poison_dmg = (i64::from(expected_ticks)
                        * (2 * i64::from(enemy_poison) - i64::from(expected_ticks) + 1))
                        / 2;
                    let effective_poison_hp = i64::from(hp).min(expected_poison_dmg);
                    if is_reviving_enemy {
                        full_acc.poison_hp_reviving += effective_poison_hp;
                    } else {
                        full_acc.poison_hp_normal += effective_poison_hp;
                    }
                }

                let effective_poison_reduction = i64::from(hp).min(poison_tick_damage) as i32;
                let effective_enemy_hp = (hp - effective_poison_reduction - enemy_doom).max(0);
                if effective_enemy_hp < full_acc.focus_target_remaining_hp {
                    full_acc.focus_target_remaining_hp = effective_enemy_hp;
                }

                if !doom_lethal && !is_minion {
                    full_acc.alive_non_minion_enemies += 1;
                }

                let permanent_enemy_str = enemy_str + enemy_temp_str_recovery.max(0);
                if permanent_enemy_str > 0 {
                    full_acc.enemy_threat_power_penalty += permanent_enemy_str.min(20) * 3;
                } else if permanent_enemy_str < 0 {
                    let perm_neg_str = -permanent_enemy_str;
                    full_acc.enemy_debuff_stacks += perm_neg_str.min(12);
                    full_acc.enemy_threat_power_penalty -= perm_neg_str.min(20) * 6;
                    full_acc.enemy_strength_suppression += perm_neg_str.min(16);
                }

                let is_stunned = ec
                    .monster
                    .as_deref()
                    .and_then(|m| m.next_move_id.as_deref())
                    == Some("STUNNED");
                if is_stunned {
                    continue;
                }

                let mut vigor_remaining = enemy_vigor.max(0);
                let mut raw_enemy_incoming = 0i32;
                let mut enemy_unblocked = 0i32;
                let mut enemy_paper_cuts_loss = 0i32;
                {
                    let mut accum_hit = |base_dmg: i32, count: i32| {
                        if count <= 0 {
                            return;
                        }
                        let hit = scale_enemy_hit(
                            base_dmg,
                            enemy_str + vigor_remaining,
                            enemy_weak > 0,
                            player_vuln > 0,
                            player_intangible > 0,
                            root,
                        );
                        vigor_remaining = 0;
                        raw_enemy_incoming += hit * count;
                        if hit > 0 {
                            for _ in 0..count {
                                apply_single_enemy_hit(
                                    hit,
                                    enemy_paper_cuts,
                                    &mut full_acc.remaining_block,
                                    &mut full_acc.remaining_osty,
                                    &mut full_acc.remaining_wuppy,
                                    &mut full_acc.remaining_buffer,
                                    &mut full_acc.osty_absorbed,
                                    &mut enemy_unblocked,
                                    &mut enemy_paper_cuts_loss,
                                );
                            }
                        }
                    };
                    if let Some(cache) = intent_cache.as_deref_mut() {
                        for &(base_dmg, count) in cache.lookup_i32(state, content, eh, ec) {
                            accum_hit(base_dmg, count);
                        }
                    } else {
                        for (d, count) in content
                            .monster_attack_intent_hits(state, eh)
                            .unwrap_or_default()
                        {
                            accum_hit(d.to_i32_trunc().unwrap_or(0), count);
                        }
                    }
                }

                full_acc.enemy_attack_damage += raw_enemy_incoming;
                full_acc.incoming_damage += raw_enemy_incoming;
                full_acc.unblocked_incoming += enemy_unblocked;
                full_acc.paper_cuts_max_hp_threat += enemy_paper_cuts_loss;

                if raw_enemy_incoming == 0 {
                    full_acc.enrage_phase_threat_dmg += estimate_activated_phase_threat(
                        state,
                        ec,
                        enemy_str,
                        enemy_weak > 1,
                        player_vuln > 1,
                        player_intangible > 1,
                        alive_enemy_total_for_phase,
                        root,
                    );
                }
            }
        } else if is_creature_reviving(state, ec) {
            if WANT_FAST {
                fast_acc.reviving_enemy_count += 1;
            }
            if WANT_FULL {
                let rev_hp = ec
                    .powers
                    .iter()
                    .find(|p| p.id == PowerId::ReattachPower && p.amount > 0)
                    .map_or_else(|| 15.max(ec.max_hp / 2), |rp| rp.amount);
                full_acc.reviving_hp_penalty += rev_hp;
            }
        }
    }
    if WANT_FULL && full_acc.focus_target_remaining_hp == i32::MAX {
        full_acc.focus_target_remaining_hp = 0;
    }
}

#[inline(always)]
fn count_playable_hand_cards(
    state: &BattleState,
    content: &dyn ContentDispatcher,
    card_metas: Option<&[CardMeta]>,
    has_player_corruption: bool,
) -> u8 {
    let mut playable = 0i32;
    for &ch in &state.player.hand {
        let Some(card) = state.card(ch) else {
            continue;
        };
        let meta_opt = match card_metas.and_then(|cm| cm.get(card.id as usize)) {
            Some(m) => Some(m),
            None => content.card_meta(card.id).ok(),
        };
        let Some(meta) = meta_opt else {
            continue;
        };
        if matches!(meta.card_type, CardType::Status | CardType::Curse) {
            continue;
        }
        if card_is_playable_in_hand(state, card, meta, has_player_corruption).0 {
            playable += 1;
        }
    }
    playable.clamp(0, u8::MAX as i32) as u8
}

#[inline(always)]
fn compute_relic_guidance_score(satisfied_priority: i32, distance: i32) -> i64 {
    let p1 = i64::from(satisfied_priority & 0xF);
    let p2 = i64::from((satisfied_priority >> 4) & 0xF);
    let p3 = i64::from((satisfied_priority >> 8) & 0xF);
    let mut prio_score = p3 * 20_000 + p2 * 9_500 + p1 * 3_000;
    if prio_score > 24_000 {
        prio_score = 35_000.min(24_000 + ((prio_score - 24_000) >> 4));
    }
    prio_score - i64::from(distance.clamp(0, 30)) * 400
}

#[inline(always)]
const fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn eval_build_fast_summary(
    state: &BattleState,
    content: &dyn ContentDispatcher,
    root: &RootEvalBaseline,
    stand_pat_hp: Option<i32>,
    pc_dead: bool,
    player_hp: i32,
    player_max_hp: i32,
    player_block: i32,
    cumulative_hp_lost: i32,
    fast_acc: EnemyFastEvalAcc,
) -> FastEvalSummary {
    let mut enemy_hp = fast_acc.enemy_hp;
    let mut raw_enemy_hp = fast_acc.raw_enemy_hp;
    let enemy_block = fast_acc.enemy_block;
    let alive_enemy_count = fast_acc.alive_enemy_count;
    let reviving_enemy_count = fast_acc.reviving_enemy_count;
    let alive_enemy_mask = fast_acc.alive_enemy_mask;
    let outstanding_stolen = fast_acc.outstanding_stolen;

    let forfeits_gold = eval_forfeits_gold(state);

    let has_pending_choice = state.pending.is_some();
    let player_dead =
        pc_dead || (!state.is_in_progress && !state.won && !state.battleworn_dummy_timed_out);
    let is_event_defeat = !player_dead && state.battleworn_dummy_timed_out;
    if is_event_defeat {
        for &eh in &state.escaped_handles {
            if let Some(ec) = state.creature(eh) {
                let hp = ec.current_hp.max(0);
                enemy_hp += hp;
                raw_enemy_hp += hp;
            }
        }
    }
    let all_enemies_dead = !is_event_defeat && alive_enemy_count == 0 && reviving_enemy_count == 0;
    let won = !player_dead
        && !is_event_defeat
        && !has_pending_choice
        && (!state.is_in_progress && state.won || all_enemies_dead);

    let projected_player_hp = if won {
        player_hp
    } else if player_dead {
        0
    } else {
        stand_pat_hp.unwrap_or(player_hp)
    };

    let death_save_use_count = i32::from(state.player.death_save_use_count);
    let death_save_hp_restored = state
        .player
        .death_save_relic_hp_restored
        .saturating_add(state.player.death_save_potion_hp_restored);

    let live_deck_size =
        (state.player.hand.len() + state.player.draw.len() + state.player.discard.len()) as i32;
    let live_deck_clutter = eval_live_deck_clutter(state, content, live_deck_size);

    let card_metas = state.hook_cache.cached_tables.map(|t| t.card_metas);
    let has_player_corruption = state.player_creature().is_some_and(|pc| {
        pc.powers
            .iter()
            .any(|p| p.id == PowerId::CorruptionPower && p.amount > 0)
    });
    let playable_hand_count =
        count_playable_hand_cards(state, content, card_metas, has_player_corruption);

    let recovered_hp = (player_hp - root.initial_player_hp
        + (cumulative_hp_lost - root.initial_cumulative_hp_lost))
        .max(0);

    let (raw_growth_reward_count, growth_rewards, growth_hp_credit, growth_reward_count) =
        if state.semantic_history.growth_rewards.is_zero()
            && root.initial_growth_rewards.is_zero()
        {
            (0, GrowthCounters::default(), 0, 0)
        } else {
            let mut raw_growth = state.semantic_history.growth_rewards;
            let delta_ms = (raw_growth.mad_science - root.initial_growth_rewards.mad_science)
                .clamp(0, root.mad_science_upgrade_capacity);
            raw_growth.mad_science = root.initial_growth_rewards.mad_science + delta_ms;
            let raw_cnt = raw_growth.total();
            if root.ignore_long_term_rewards {
                (raw_cnt, GrowthCounters::default(), 0, 0)
            } else {
                let credit = root.effective_growth_budgets.credit(&raw_growth);
                (raw_cnt, raw_growth, credit, raw_cnt)
            }
        };

    let relic_eval = if root.active_relic_targets.is_empty() {
        RelicCounterEvaluation::default()
    } else {
        RelicCounterPolicy::evaluate(
            state,
            &root.active_relic_targets,
            won,
            root.post_combat_relic_heal,
            root.boss_hp_relief,
        )
    };
    let strategy_goal_hp_credit = growth_hp_credit.saturating_add(relic_eval.hp_credit);
    let strategic_hp_credit = strategy_goal_hp_credit.saturating_add(relic_eval.healing_hp_credit);
    let strategy_goal_count = growth_reward_count.saturating_add(relic_eval.satisfied_priority);
    let all_growth_targets_satisfied = !root.effective_has_growth_targets
        || root.growth_opportunity_targets.is_satisfied_by(&growth_rewards);
    let all_strategy_targets_satisfied =
        all_growth_targets_satisfied && relic_eval.all_satisfied();
    let brightest_flame_max_hp_spent = root.brightest_flame_spent(state);
    let feed_count = state
        .semantic_history
        .growth_rewards
        .feed
        .clamp(0, u8::MAX as i32) as u8;

    let base_score = if player_dead || projected_player_hp <= 0 {
        weights::DEATH_PENALTY
            + state.player.turn_number as f64 * 100_000.0
            - enemy_hp as f64 * 100.0
            - (alive_enemy_count + reviving_enemy_count) as f64 * 50_000.0
    } else if is_event_defeat {
        -500_000_000_000.0
            - death_save_use_count as f64 * 500_000_000.0
            - enemy_hp as f64 * 10_000.0
            - cumulative_hp_lost as f64 * 50_000.0
    } else if won {
        let relic_heal = ActEndingBossPolicy::ranked_post_combat_relic_heal(
            root.post_combat_relic_heal,
            true,
            projected_player_hp,
            player_max_hp,
        );
        let net_hp_delta_centi = ActEndingBossPolicy::compute_net_persistent_hp_delta_centi(
            projected_player_hp,
            relic_heal,
            root.initial_player_hp,
            player_max_hp,
            root.initial_player_max_hp,
            death_save_hp_restored,
            strategic_hp_credit,
            root.boss_hp_relief,
            root.ignore_long_term_rewards,
            i32::from(feed_count) * root.feed_max_hp_per_kill,
        );
        let mut s = weights::VICTORY_BONUS
            - death_save_use_count as f64 * 500_000_000.0
            + net_hp_delta_centi as f64 * 390.0
            + net_hp_delta_centi as f64 * 2.0
            - state.player.turn_number as f64 * 5_000.0;
        if !root.ignore_long_term_rewards && growth_reward_count > 0 {
            s += growth_reward_count as f64
                * if root.effective_has_growth_targets {
                    25_000.0
                } else {
                    5_000.0
                };
        }
        if !root.active_relic_targets.is_empty() {
            s += compute_relic_guidance_score(relic_eval.satisfied_priority, relic_eval.distance)
                as f64;
        }
        if root.theft_policy == SolverTheftPolicy::PreserveResources {
            s -= outstanding_stolen as f64 * 1_000_000.0;
            if forfeits_gold {
                s -= 2_000_000.0;
            }
        }
        s
    } else {
        let ongoing_net_hp_delta_centi = ActEndingBossPolicy::compute_net_persistent_hp_delta_centi(
            projected_player_hp,
            0,
            root.initial_player_hp,
            player_max_hp,
            root.initial_player_max_hp,
            death_save_hp_restored,
            strategic_hp_credit,
            BossHpRelief::None,
            root.ignore_long_term_rewards,
            i32::from(feed_count) * root.feed_max_hp_per_kill,
        );
        let mut s = ongoing_net_hp_delta_centi as f64 * 390.0
            - death_save_use_count as f64 * 500_000_000.0
            - enemy_hp as f64 * 10_000.0;
        if !root.active_relic_targets.is_empty() {
            s += compute_relic_guidance_score(relic_eval.satisfied_priority, relic_eval.distance)
                as f64;
        }
        s
    };

    let empty_potion_slots = state
        .player
        .potion_slots
        .iter()
        .take(16)
        .filter(|s| s.is_none())
        .count() as u8;
    let alchemize_count = state.semantic_history.growth_rewards.alchemize.clamp(0, 255) as u8;

    FastEvalSummary {
        player_hp,
        player_max_hp,
        player_block,
        projected_player_hp,
        cumulative_hp_lost,
        recovered_hp,
        enemy_hp,
        raw_enemy_hp,
        enemy_block,
        alive_enemy_count,
        reviving_enemy_count,
        alive_enemy_mask,
        player_dead,
        all_enemies_dead,
        won,
        is_event_defeat,
        has_pending_choice,
        death_save_use_count,
        death_save_hp_restored,
        growth_hp_credit,
        growth_reward_count,
        raw_growth_reward_count,
        growth_rewards,
        relic_eval,
        strategy_goal_hp_credit,
        strategic_hp_credit,
        strategy_goal_count,
        all_strategy_targets_satisfied,
        brightest_flame_max_hp_spent,
        energy: state.player.energy,
        stars: state.player.stars,
        hand_count: state.player.hand.len() as i32,
        playable_hand_count,
        live_deck_size,
        live_deck_clutter,
        alchemize_count,
        empty_potion_slots,
        feed_count,
        outstanding_stolen,
        forfeits_gold,
        combat_ended_turn: if won {
            Some(state.player.turn_number)
        } else {
            None
        },
        base_score,
    }
}

/// Fast single-pass scan over `BattleState` with zero heap allocations.
pub fn evaluate_fast(
    state: &BattleState,
    content: &dyn ContentDispatcher,
    root: &RootEvalBaseline,
    stand_pat_hp: Option<i32>,
) -> FastEvalSummary {
    let pc = state.player_creature();
    let player_hp = pc.map_or(state.player.current_hp, |c| c.current_hp);
    let player_max_hp = pc.map_or(state.player.max_hp, |c| c.max_hp);
    let player_block = pc.map_or(state.player.block, |c| c.block);
    let cumulative_hp_lost = pc.map_or(state.player.cumulative_hp_lost, |c| c.cumulative_hp_lost);
    let pc_dead = pc.is_none_or(|c| c.is_dead());

    let mut fast_acc = EnemyFastEvalAcc::default();
    let mut dummy_full = EnemyFullEvalAcc::default();
    eval_enemies_pass::<true, false>(
        state,
        content,
        root,
        None,
        0,
        0,
        0,
        None,
        &mut fast_acc,
        &mut dummy_full,
    );

    eval_build_fast_summary(
        state,
        content,
        root,
        stand_pat_hp,
        pc_dead,
        player_hp,
        player_max_hp,
        player_block,
        cumulative_hp_lost,
        fast_acc,
    )
}

/// Stage-2 full evaluation details materialized lazily ONLY for nodes that survive TT & pruning.
#[derive(Clone, Copy, Debug, Default)]
pub struct FullEvalDetails {
    pub persistent_buff_value: i32,
    pub setup_score: i32,
    pub expected_regen_heal: i32,
    pub board_investment: i32,
    pub enemy_debuff_value: i32,
    pub enemy_threat_power_penalty: i32,
    pub orb_score: i32,
    pub orb_capacity: i32,
    pub max_dark_evoke: i32,
    pub total_poison_and_doom: i32,
    pub doom_lethal_count: i32,
    pub effective_doom_value: i32,
    pub poison_lethal_count: i32,
    pub poison_lethal_hp_offset: i32,
    pub poison_hp_normal: i64,
    pub poison_hp_reviving: i64,
    pub next_turn_stars: i32,
    pub next_turn_summon: i32,
    pub latent_setup_value: i32,
    pub replay_potential_value: i32,
    pub retained_attack_value: i32,
    pub reachable_hand_value: i32,
    pub future_resource_value: i32,
    pub delayed_damage_value: i32,
    pub reactive_damage_value: i32,
    pub enemy_strength_suppression: i32,
    pub enemy_weak_turns: i32,
    pub enemy_vulnerable_turns: i32,
    pub focus_target_pressure: i32,
    pub focus_target_remaining_hp: i32,
    pub osty_hp: i32,
    pub osty_max_hp: i32,
    pub osty_absorbed: i32,
    pub wuppy_hp: i32,
    pub min_sandpit_turns: i32,
    pub furnace_power: i32,
    pub retained_blade_bonus_base: i64,
    pub forged_blade_bonus_std: i64,
    pub forged_blade_bonus_surv: i64,
    pub retained_hold_bonus: i64,
    pub cost_zeroed_hand_bonus: i64,
    pub excess_current_block_scaler: i32,
    pub ready_arrows: i32,
    pub melody_cards_finished: i32,
    pub playable_attack_count: i32,
    pub incoming_damage: i32,
    pub end_turn_block: i32,
    pub effective_defense: i32,
    pub unblocked_incoming: i32,
    pub remaining_block: i32,
    pub paper_cuts_max_hp_threat: i32,
    pub enrage_phase_threat_dmg: i32,
    pub player_intangible: i32,
    pub paels_legion_ready: bool,
    pub reviving_hp_penalty: i32,
    pub all_reattach_encounter: bool,
    pub highest_alive_enemy_hp: i32,
    pub preserves_excess_block: bool,
    pub next_turn_block: i32,
    pub next_turn_energy: i32,
    pub next_turn_draw: i32,
    pub playable_card_count: i32,
    pub zero_cost_playable_count: i32,
    pub hand_clutter: i32,
    pub wasted_engine_exhausts: i32,
    pub missed_the_hunt_rewards: i32,
    pub alive_non_minion_enemies: i32,
    pub active_growth_hand_budget: Option<i32>,
    pub active_growth_deck_budget: Option<i32>,
    pub anger_copies_generated: i32,
    pub enemy_combat_dist_hash: u64,
    pub unordered_pile_hash: u64,
    pub tactical_sig: u64,
    pub best_power_priority: PowerRoutePriority,
    pub beam_rank_score: f64,
    pub rollout_score: Option<f64>,
    pub choice_lookahead_score: Option<f64>,
    pub has_choice_lookahead: bool,
}

/// Compact Stage-2 fields retained in `NodeArena::full_evals` across waves.
#[derive(Clone, Copy, Debug, Default)]
pub struct StoredNodeEval {
    pub beam_rank_score: f64,
    pub rollout_score: Option<f64>,
    pub choice_lookahead_score: Option<f64>,
    pub tactical_sig: u64,
    pub effective_defense: i32,
    pub board_investment: i32,
    pub setup_score: i32,
    pub optionality_cards: i32,
    pub persistent_buff_value: i32,
    pub reachable_hand_value: i32,
    pub max_dark_evoke: i32,
    pub total_poison_and_doom: i32,
    pub best_power_priority: PowerRoutePriority,
    pub has_choice_lookahead: bool,
}

impl StoredNodeEval {
    #[inline]
    pub fn effective_turn_boundary_score(&self) -> f64 {
        match self.rollout_score {
            Some(rs) => 0.35 * self.beam_rank_score + 0.65 * rs,
            None => self.beam_rank_score,
        }
    }
}

impl FullEvalDetails {
    #[inline]
    pub fn to_stored(&self) -> StoredNodeEval {
        StoredNodeEval {
            beam_rank_score: self.beam_rank_score,
            rollout_score: self.rollout_score,
            choice_lookahead_score: self.choice_lookahead_score,
            tactical_sig: self.tactical_sig,
            effective_defense: self.effective_defense,
            board_investment: self.board_investment,
            setup_score: self.setup_score,
            optionality_cards: self.playable_card_count + self.next_turn_draw,
            persistent_buff_value: self.persistent_buff_value,
            reachable_hand_value: self.reachable_hand_value,
            max_dark_evoke: self.max_dark_evoke,
            total_poison_and_doom: self.total_poison_and_doom,
            best_power_priority: self.best_power_priority,
            has_choice_lookahead: self.has_choice_lookahead,
        }
    }

    #[inline]
    pub fn effective_turn_boundary_score(&self) -> f64 {
        match self.rollout_score {
            Some(rs) => 0.35 * self.beam_rank_score + 0.65 * rs,
            None => self.beam_rank_score,
        }
    }
}

#[inline]
fn catalog_power_setup_weight(id: PowerId) -> i32 {
    match id {
        PowerId::AccuracyPower => 18,
        PowerId::AimPower => 16,
        PowerId::ArtifactPower => 16,
        PowerId::ChildOfTheStarsPower => 22,
        PowerId::ConductorsBatonPower => 6,
        PowerId::CorruptionPower => 40,
        PowerId::CreativeAiPower => 32,
        PowerId::CuriousPower => 12,
        PowerId::DanseMacabrePower => 24,
        PowerId::DarkEmbracePower => 28,
        PowerId::DemesnePower => 28,
        PowerId::DexterityPower => 18,
        PowerId::DoubleDamagePower => 22,
        PowerId::EnvenomPower => 24,
        PowerId::ExtraTurnPower => 28,
        PowerId::FeelNoPainPower => 24,
        PowerId::FlameBarrierPower => 12,
        PowerId::FocusPower => 24,
        PowerId::GenesisPower => 28,
        PowerId::GigantificationPower => 18,
        PowerId::HelloWorldPower => 14,
        PowerId::InfiniteBladesPower => 18,
        PowerId::JuggernautPower => 20,
        PowerId::LethalityPower => 20,
        PowerId::LoopPower => 24,
        PowerId::LullabyPower => 20,
        PowerId::MachineLearningPower => 20,
        PowerId::NightmarePower => 28,
        PowerId::NoxiousFumesPower => 22,
        PowerId::PaleBlueDotPower => 24,
        PowerId::QuietusPower => 12,
        PowerId::RagePower => 10,
        PowerId::ReboundPower => 6,
        PowerId::RegenPower => 16,
        PowerId::RitualPower => 28,
        PowerId::RupturePower => 18,
        PowerId::ShadowmeldPower => 12,
        PowerId::ShowOffPower => 18,
        PowerId::ShroudPower => 22,
        PowerId::SleightOfFleshPower => 20,
        PowerId::SoulboundPower => 16,
        PowerId::SpectrumShiftPower => 22,
        PowerId::SpinnerPower => 20,
        PowerId::StormPower => 22,
        PowerId::StrengthPower => 18,
        PowerId::SwordSagePower => 24,
        PowerId::TheSealedThronePower => 30,
        PowerId::ToolsOfTheTradePower => 22,
        PowerId::TyrannyPower => 24,
        PowerId::ViciousPower => 22,
        PowerId::VigorPower => 8,
        PowerId::WellLaidPlansPower => 26,
        PowerId::YahPower => 20,
        _ => 0,
    }
}

#[inline]
fn compute_player_power_setup_weight(
    id: PowerId,
    amount: i32,
    meta: Option<&sts2_types::PowerMeta>,
) -> i32 {
    if meta.is_some_and(|m| m.power_type == PowerType::Debuff) {
        let raw_mag = amount.abs().max(1);
        let mag = raw_mag.clamp(1, 6);
        return match id {
            PowerId::WeakPower => -mag * 3,
            PowerId::VulnerablePower => -mag * 3,
            PowerId::FrailPower => -mag * 3,
            PowerId::WraithFormPower => -mag * 2,
            PowerId::BiasedCognitionPower => -mag * 2,
            PowerId::DisintegrationPower => -(raw_mag * 8).min(80),
            PowerId::ConstrictPower => -(raw_mag * 8).min(80),
            PowerId::WasteAwayPower => -(raw_mag * 28).min(84),
            PowerId::AmnesiaPower => -(raw_mag * 12).min(36),
            PowerId::MindRotPower => -(raw_mag * 5).min(25),
            PowerId::NoBlockPower => -(raw_mag * 8).min(32),
            PowerId::HexPower => -10,
            PowerId::TangledPower => -(raw_mag * 6).min(18),
            PowerId::NoDrawPower => -5,
            PowerId::NoEnergyGainPower => -6,
            PowerId::BorrowedTimePower => -(raw_mag * 4).min(16),
            PowerId::ChainsOfBindingPower => -(raw_mag * 4).min(16),
            PowerId::SlothPower => -((6 - raw_mag.min(5)) * 5).max(6),
            PowerId::SlowPower => -2,
            PowerId::NeurosurgePower => -(raw_mag * 6).min(30),
            PowerId::DemisePower => -(raw_mag * 5).min(40),
            PowerId::StranglePower => -(raw_mag * 4).min(24),
            PowerId::TenderPower => -6,
            PowerId::DampenPower => -10,
            PowerId::RingingPower => -6,
            PowerId::SmoggyPower => -8,
            PowerId::SurroundedPower => -8,
            PowerId::YouAreMinePower => -8,
            PowerId::ConfusedPower => -4,
            PowerId::TheGambitPower => -4,
            _ => -mag.min(3),
        };
    }

    match id {
        PowerId::BlockNextTurnPower
        | PowerId::SelfFormingClayPower
        | PowerId::ToricToughnessPower => return (amount.max(0) / 4).min(14),
        PowerId::EnergyNextTurnPower => return (amount.max(0).min(4)) * 3,
        PowerId::DrawCardsNextTurnPower => return (amount.max(0).min(5)) * 2,
        PowerId::StarNextTurnPower | PowerId::SummonNextTurnPower => return (amount.max(0).min(4)) * 2,
        _ => {}
    }

    let pos_amt = amount.clamp(0, 16);
    let explicit_weight = match id {
        PowerId::VoidFormPower => pos_amt.min(3) * 20,
        PowerId::SerpentFormPower => pos_amt.min(12) * 3,
        PowerId::SpeedsterPower => pos_amt.min(10) * 2,
        PowerId::SpiritOfAshPower => pos_amt.min(12) * 2,
        PowerId::VeilpiercerPower => pos_amt.min(4) * 5,
        PowerId::MayhemPower => pos_amt.min(3) * 10,
        PowerId::AutomationPower => pos_amt.min(3) * 6,
        PowerId::ReaperFormPower => pos_amt.min(3) * 12,
        PowerId::FurnacePower => 8 + pos_amt.min(12) * 2,
        PowerId::OrbitPower => pos_amt.min(4) * 12,
        PowerId::PagestormPower => pos_amt.min(4) * 10,
        PowerId::PlatingPower => pos_amt.min(16) * 2,
        PowerId::ThornsPower => pos_amt.min(12) * 2,
        PowerId::IntangiblePower => pos_amt.min(3) * 16,
        PowerId::EchoFormPower => pos_amt.min(3) * 16,
        PowerId::DemonFormPower => pos_amt.min(6) * 6,
        PowerId::BufferPower => pos_amt.min(5) * 10,
        PowerId::AfterimagePower | PowerId::DuplicationPower => pos_amt.min(4) * 8,
        PowerId::BarricadePower => 14,
        PowerId::BlurPower => pos_amt.min(4) * 5,
        PowerId::CalcifyPower | PowerId::PhantomBladesPower | PowerId::ThunderPower => {
            pos_amt.min(10) * 2
        }
        PowerId::DevourLifePower
        | PowerId::HauntPower
        | PowerId::NecroMasteryPower
        | PowerId::ArsenalPower
        | PowerId::RadiancePower => pos_amt.min(6) * 4,
        PowerId::CountdownPower
        | PowerId::BlackHolePower
        | PowerId::PillarOfCreationPower
        | PowerId::CoolantPower
        | PowerId::CrimsonMantlePower
        | PowerId::InfernoPower => pos_amt.min(8) * 3,
        PowerId::MonarchsGazePower
        | PowerId::StratagemPower
        | PowerId::NostalgiaPower
        | PowerId::FeralPower
        | PowerId::IterationPower
        | PowerId::EntropyPower
        | PowerId::FreeSkillPower
        | PowerId::FreeAttackPower => pos_amt.min(4) * 5,
        PowerId::SeekingEdgePower | PowerId::MasterPlannerPower | PowerId::HellraiserPower => 10,
        PowerId::HammerTimePower => 12,
        PowerId::ParryPower => pos_amt.min(8) * 2,
        PowerId::CrueltyPower => pos_amt.min(6) * 3,
        PowerId::AccelerantPower => pos_amt.min(4) * 4,
        PowerId::FanOfKnivesPower => 8,
        PowerId::PanachePower => pos_amt.min(16),
        PowerId::SubroutinePower
        | PowerId::CalamityPower
        | PowerId::BurstPower
        | PowerId::SignalBoostPower
        | PowerId::OneTwoPunchPower
        | PowerId::FreePowerPower => pos_amt.min(4) * 6,
        PowerId::RetainHandPower => pos_amt.min(4) * 5,
        PowerId::UnmovablePower => pos_amt.min(3) * 14,
        PowerId::FriendshipPower | PowerId::PyrePower => pos_amt.min(4) * 18,
        PowerId::SentryModePower => pos_amt.min(4) * 10,
        PowerId::LightningRodPower | PowerId::CallOfTheVoidPower | PowerId::MotifPower => {
            pos_amt.min(4) * 8
        }
        PowerId::CorrosiveWavePower
        | PowerId::ArcheryPower
        | PowerId::HiddenRootsPower => pos_amt.min(8) * 3,
        PowerId::StampedePower => pos_amt.min(4) * 12,
        PowerId::ShadowStepPower => pos_amt.min(4) * 10,
        PowerId::JugglingPower
        | PowerId::AggressionPower
        | PowerId::MythicFormPower
        | PowerId::LostLutePower => pos_amt.min(3) * 12,
        PowerId::CacophonyPower
        | PowerId::HailstormPower
        | PowerId::ExplosiveArrowsPower
        | PowerId::SongPathPower
        | PowerId::ViolentChordPower
        | PowerId::GodByrdPower => pos_amt.min(12) * 2,
        PowerId::ClarityPower | PowerId::ForegoneConclusionPower => pos_amt.min(4) * 5,
        PowerId::ColossusPower | PowerId::ReflectPower | PowerId::EcstasyPower => {
            pos_amt.min(4) * 6
        }
        PowerId::ConcoctPower | PowerId::MonologuePower => pos_amt.min(6) * 2,
        PowerId::ConsumingShadowPower | PowerId::SimpletonPower => pos_amt.min(3) * 10,
        PowerId::FastenPower
        | PowerId::PrepTimePower
        | PowerId::SmokestackPower
        | PowerId::AllIsWellPower => pos_amt.min(10) * 2,
        PowerId::RollingBoulderPower => 10 + pos_amt.min(15),
        PowerId::TheBombPower
        | PowerId::TrashToTreasurePower
        | PowerId::EnnuiPower
        | PowerId::KiVersePower
        | PowerId::TunePower => pos_amt.min(3) * 8,
        PowerId::TrackingPower | PowerId::SerratedArrowsPower => {
            (amount.max(0) / 25).min(4) * 6
        }
        PowerId::HoningPower => (amount.max(0) / 25).min(4) * 5,
        PowerId::StretchPower => pos_amt.min(16),
        PowerId::CompostPower => pos_amt.min(3) * 6,
        _ => 0,
    };
    if explicit_weight != 0 {
        return explicit_weight.clamp(-24, 48);
    }

    let catalog_weight = catalog_power_setup_weight(id);
    if catalog_weight != 0 {
        if meta.is_some_and(|m| m.stack_type == PowerStackType::Single) {
            return (catalog_weight / 2).clamp(-24, 28);
        }
        let clamped_amt = amount.clamp(-10, 10);
        return ((catalog_weight * clamped_amt) / 3).clamp(-24, 28);
    }

    0
}

#[inline]
fn card_base_heuristic_value(card_id: CardId, upgrade_level: i32, meta_type: CardType) -> i32 {
    let upg = if upgrade_level > 0 { 3 } else { 0 };
    match card_id {
        CardId::Bash => 8 + upg,
        CardId::Neutralize => 4 + upg,
        CardId::Survivor => 8 + upg,
        CardId::Bodyguard => 5 + upg,
        CardId::Unleash => 6 + upg,
        CardId::FallingStar => 7 + upg,
        _ => match meta_type {
            CardType::Attack => 6 + upg,
            CardType::Skill => 5 + upg,
            CardType::Power => 8 + upg,
            _ => 0,
        },
    }
}

struct IntentHitsEntry {
    monster_id: MonsterId,
    move_id: Cow<'static, str>,
    hits_i32: SmallVec<[(i32, i32); 2]>,
    #[cfg(debug_assertions)]
    hits: SmallVec<[(Dec, i32); 2]>,
}

pub struct IntentHitsCache {
    game_mode: Option<GameMode>,
    ascension: u8,
    last_pos: u8,
    fast_idx: [u8; 32],
    entries: Vec<IntentHitsEntry>,
    dyn_buf: SmallVec<[(i32, i32); 4]>,
}

impl Default for IntentHitsCache {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl IntentHitsCache {
    #[inline]
    pub fn new() -> Self {
        Self {
            game_mode: None,
            ascension: 0,
            last_pos: u8::MAX,
            fast_idx: [u8::MAX; 32],
            entries: Vec::with_capacity(16),
            dyn_buf: SmallVec::new(),
        }
    }

    #[inline]
    pub fn clear(&mut self) {
        self.game_mode = None;
        self.last_pos = u8::MAX;
        self.fast_idx.fill(u8::MAX);
        self.entries.clear();
        self.dyn_buf.clear();
    }

    #[inline(always)]
    fn hint_slot(mid: MonsterId, mb: &[u8]) -> usize {
        let len = mb.len();
        let b0 = mb.first().copied().unwrap_or(0) as usize;
        let b_last = mb.last().copied().unwrap_or(0) as usize;
        ((mid as usize).wrapping_mul(31) ^ (len << 3) ^ b0 ^ b_last.wrapping_mul(7)) & 31
    }

    #[inline]
    pub(crate) fn lookup_i32<'a>(
        &'a mut self,
        state: &BattleState,
        content: &dyn ContentDispatcher,
        eh: CreatureHandle,
        ec: &CreatureInst,
    ) -> &'a [(i32, i32)] {
        let Some(m) = &ec.monster else {
            return &[];
        };
        let mid = m.monster_id;
        if matches!(
            mid,
            MonsterId::WaterfallGiant | MonsterId::TestSubject | MonsterId::TheForgotten
        ) {
            self.dyn_buf.clear();
            for (d, c) in content.monster_attack_intent_hits(state, eh).unwrap_or_default() {
                self.dyn_buf.push((d.to_i32_trunc().unwrap_or(0), c));
            }
            return self.dyn_buf.as_slice();
        }
        let gm = state.static_ctx.game_mode;
        let asc = state.static_ctx.ascension;
        if self.game_mode != Some(gm) || self.ascension != asc {
            self.game_mode = Some(gm);
            self.ascension = asc;
            self.last_pos = u8::MAX;
            self.fast_idx.fill(u8::MAX);
            self.entries.clear();
        }
        let move_cow = m.next_move_id.as_ref().unwrap_or(&m.current_state_id);
        let mb = move_cow.as_bytes();
        let lp = self.last_pos as usize;
        let mru_hit = lp < self.entries.len() && {
            let e = &self.entries[lp];
            let eb = e.move_id.as_bytes();
            e.monster_id == mid && eb.len() == mb.len() && eb.as_ptr() == mb.as_ptr()
        };
        if mru_hit {
            let entry = &self.entries[lp];
            #[cfg(debug_assertions)]
            {
                let fresh = content
                    .monster_attack_intent_hits(state, eh)
                    .unwrap_or_default();
                debug_assert_eq!(
                    entry.hits,
                    fresh,
                    "IntentHitsCache mismatch for {mid:?} move={}",
                    move_cow.as_ref()
                );
            }
            return entry.hits_i32.as_slice();
        }
        let hint_slot = Self::hint_slot(mid, mb);
        let hint_pos = self.fast_idx[hint_slot] as usize;
        let pos_opt = if hint_pos < self.entries.len() {
            let e = &self.entries[hint_pos];
            let eb = e.move_id.as_bytes();
            if e.monster_id == mid
                && eb.len() == mb.len()
                && (eb.as_ptr() == mb.as_ptr() || eb == mb)
            {
                Some(hint_pos)
            } else {
                self.entries.iter().position(|e| {
                    let eb = e.move_id.as_bytes();
                    e.monster_id == mid
                        && eb.len() == mb.len()
                        && (eb.as_ptr() == mb.as_ptr() || eb == mb)
                })
            }
        } else {
            self.entries.iter().position(|e| {
                let eb = e.move_id.as_bytes();
                e.monster_id == mid
                    && eb.len() == mb.len()
                    && (eb.as_ptr() == mb.as_ptr() || eb == mb)
            })
        };
        if let Some(pos) = pos_opt {
            self.last_pos = pos as u8;
            self.fast_idx[hint_slot] = pos as u8;
            let entry = &self.entries[pos];
            #[cfg(debug_assertions)]
            {
                let fresh = content
                    .monster_attack_intent_hits(state, eh)
                    .unwrap_or_default();
                debug_assert_eq!(
                    entry.hits,
                    fresh,
                    "IntentHitsCache mismatch for {mid:?} move={}",
                    move_cow.as_ref()
                );
            }
            return entry.hits_i32.as_slice();
        }
        let hits = content
            .monster_attack_intent_hits(state, eh)
            .unwrap_or_default();
        let mut hits_i32: SmallVec<[(i32, i32); 2]> = SmallVec::with_capacity(hits.len());
        for &(d, c) in &hits {
            hits_i32.push((d.to_i32_trunc().unwrap_or(0), c));
        }
        if self.entries.len() < 64 {
            let pos = self.entries.len();
            self.entries.push(IntentHitsEntry {
                monster_id: mid,
                move_id: move_cow.clone(),
                hits_i32,
                #[cfg(debug_assertions)]
                hits,
            });
            self.last_pos = pos as u8;
            self.fast_idx[hint_slot] = pos as u8;
            return self.entries.last().unwrap().hits_i32.as_slice();
        }
        self.dyn_buf.clear();
        self.dyn_buf.extend_from_slice(&hits_i32);
        self.dyn_buf.as_slice()
    }
}

#[inline(always)]
fn tracked_fatal_growth_budget(card_id: CardId, root: &RootEvalBaseline) -> Option<i32> {
    let b = &root.effective_growth_budgets;
    match card_id {
        CardId::Feed => Some(b.feed.max(0)),
        CardId::HandOfGreed => Some(b.hand_of_greed.max(0)),
        CardId::TheHunt => Some(b.the_hunt.max(0)),
        _ => None,
    }
}

#[inline(always)]
fn read_sovereign_blade_damage(state: &BattleState, card: &CardInst) -> i32 {
    state
        .card_ext(card.ext_index)
        .and_then(|e| e.private_fields.get("SovereignBlade._damage"))
        .and_then(|fv| match fv {
            FieldValue::Decimal(d) => d.to_i32_trunc().ok(),
            FieldValue::Int(x) => Some(*x),
            _ => None,
        })
        .unwrap_or(10)
        .max(0)
}

#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn eval_finalize_full_details(
    state: &BattleState,
    content: &dyn ContentDispatcher,
    fast: &FastEvalSummary,
    root: &RootEvalBaseline,
    card_metas: Option<&[CardMeta]>,
    card_rollout_profiles: Option<&[CardRolloutProfile]>,
    pre: PlayerPreEnemyState,
    e_acc: EnemyFullEvalAcc,
) -> FullEvalDetails {
    let mut p_acc = pre.p_acc;
    let mut persistent_buff_value = p_acc.persistent_buff_value;

    let orb_capacity = pre.orb_acc.orb_capacity;
    let orb_score = pre.orb_acc.orb_score;
    let dark_evoke_sum = pre.orb_acc.dark_evoke_sum;
    let max_dark_evoke = pre.orb_acc.max_dark_evoke;
    let osty_hp = pre.osty_hp;
    let osty_max_hp = pre.osty_max_hp;
    let wuppy_hp = pre.wuppy_hp;
    let end_turn_block = pre.end_turn_block;
    let pet_defense_buffer = pre.pet_defense_buffer;

    // Cross-turn relic carryovers & PaelsLegion / PaelsEye / RunicPyramid readiness.
    let mut paels_legion_ready = false;
    let mut paels_eye_triggered = false;
    let mut has_runic_pyramid = false;
    for r in &state.player.relics {
        if r.is_melted {
            continue;
        }
        match r.id {
            RelicId::ArtOfWar if state.current_side == CombatSide::Player => {
                let atk_played = matches!(
                    r.private_fields.get("ArtOfWar._attackPlayedThisTurn"),
                    Some(FieldValue::Bool(true))
                );
                if !atk_played && state.player.counters.attack_cards_played_this_turn == 0 {
                    p_acc.next_turn_energy += 1;
                }
            }
            RelicId::Pocketwatch if state.current_side == CombatSide::Player => {
                let played_field = match r.private_fields.get("Pocketwatch._cardsPlayedThisTurn") {
                    Some(FieldValue::Int(x)) => *x,
                    _ => 0,
                };
                if played_field <= 3 && state.player.counters.card_plays_started_this_turn <= 3 {
                    p_acc.next_turn_draw += 3;
                }
            }
            RelicId::PaelsLegion => {
                let cooldown = match r.private_fields.get("PaelsLegion._cooldown") {
                    Some(FieldValue::Int(x)) => *x,
                    _ => 0,
                };
                let ordinal_active = r
                    .private_fields
                    .get("PaelsLegion._affectedCardPlayOrdinal")
                    .is_some();
                if e_acc.enemy_attack_damage == 0 && cooldown <= 0 && !ordinal_active {
                    paels_legion_ready = true;
                }
            }
            RelicId::PaelsEye => {
                if matches!(
                    r.private_fields.get("PaelsEye._usedThisCombat"),
                    Some(FieldValue::Bool(true))
                ) {
                    paels_eye_triggered = true;
                }
            }
            RelicId::RunicPyramid => {
                has_runic_pyramid = true;
            }
            _ => {}
        }
    }

    if e_acc.min_sandpit_turns < i32::MAX && fast.enemy_hp > 40 {
        persistent_buff_value += e_acc.min_sandpit_turns.min(6) * 8;
    }
    if orb_capacity > 0 {
        persistent_buff_value += orb_capacity * 2;
    }
    if orb_score > 0 {
        persistent_buff_value += soft_cap_positive(orb_score / 3, 24, 72);
    }
    if osty_hp > 0 {
        persistent_buff_value += soft_cap_positive(osty_hp / 4, 12, 48);
    }

    let mut latent_setup_value = 0i32;
    let mut replay_potential_value = 0i32;
    let mut retained_attack_value = 0i32;
    let mut reachable_hand_value = 0i32;
    let mut future_resource_value = p_acc.future_resource_value;
    let mut playable_attack_count = 0i32;
    let mut best_power_priority = PowerRoutePriority::Low;
    let mut playable_card_count = 0i32;
    let mut zero_cost_playable_count = 0i32;
    let mut retained_cards = 0i32;
    let mut extra_retained_candidates = 0i32;
    let mut has_copy_or_power_in_hand = false;
    let mut has_playable_block_scaler = false;
    let mut hand_clutter = 0i32;
    let mut cost_zeroed_hand_bonus = 0i64;
    let mut retained_blade_bonus_base = 0i64;
    let mut forged_blade_bonus_std = 0i64;
    let mut forged_blade_bonus_surv = 0i64;
    let mut total_forged_delta = 0i32;
    let blade_synergy_mult = if p_acc.has_blade_synergy { 130i64 } else { 100i64 };
    let mut retained_hold_bonus = 0i64;
    let mut has_nightmare_in_hand = false;
    let track_ongoing_growth = !root.ignore_long_term_rewards
        && root.effective_has_growth_targets
        && !root.growth_opportunity_targets.is_satisfied_by(&fast.growth_rewards);
    let mut active_growth_hand_budget: Option<i32> = None;
    let mut active_growth_deck_budget: Option<i32> = None;
    let hand_retained_at_turn_end = p_acc.player_retain_hand > 0 || has_runic_pyramid;
    let mut anger_copies_generated = 0i32;

    for &ch in &state.player.hand {
        let Some(card) = state.card(ch) else {
            continue;
        };
        if card.id == CardId::Anger
            && (card.clone_of_handle.is_some() || card.temp_flags.contains(CardTempFlags::IS_DUPE))
        {
            anger_copies_generated += 1;
        }
        let meta_opt = match card_metas.and_then(|cm| cm.get(card.id as usize)) {
            Some(m) => Some(m),
            None => content.card_meta(card.id).ok(),
        };
        let Some(meta) = meta_opt else {
            continue;
        };
        if matches!(meta.card_type, CardType::Status | CardType::Curse) {
            hand_clutter += 1;
            continue;
        }
        if track_ongoing_growth {
            if let Some(gb) = tracked_fatal_growth_budget(card.id, root) {
                active_growth_hand_budget =
                    Some(active_growth_hand_budget.map_or(gb, |cur| cur.max(gb)));
                active_growth_deck_budget =
                    Some(active_growth_deck_budget.map_or(gb, |cur| cur.max(gb)));
            }
        }

        let kw = card_effective_keywords(card);
        let is_ethereal = kw.contains(CardKeywords::ETHEREAL)
            || (p_acc.has_player_hex
                && card.affliction.is_some_and(|a| a.id == AfflictionId::Hexed));
        let is_intrinsic_or_single_retain = kw.contains(CardKeywords::RETAIN)
            || card.temp_flags.contains(CardTempFlags::TEMPORARY_RETAIN_TURN);
        let is_retained =
            is_intrinsic_or_single_retain || (hand_retained_at_turn_end && !is_ethereal);

        if !is_ethereal
            && (meta.card_type == CardType::Power
                || matches!(card.id, CardId::Nightmare | CardId::DualWield))
        {
            has_copy_or_power_in_hand = true;
        }

        if is_intrinsic_or_single_retain {
            retained_cards += 1;
        } else if hand_retained_at_turn_end
            && !is_ethereal
            && meta.rarity != CardRarity::Basic
            && !kw.contains(CardKeywords::UNPLAYABLE)
        {
            extra_retained_candidates += 1;
        }

        let prof = card_rollout_profiles.and_then(|t| t.get(card.id as usize));
        if p_acc.player_retain_hand > 0
            && !is_ethereal
            && !kw.contains(CardKeywords::UNPLAYABLE)
        {
            if card.id == CardId::Nightmare {
                has_nightmare_in_hand = true;
            }
            if e_acc.unblocked_incoming == 0
                && (meta.flags.contains(CardStaticFlags::GAINS_BLOCK)
                    || prof.is_some_and(|pr| pr.base_block > 0 || pr.upg_block > 0))
            {
                let blk = prof
                    .map_or(0, |pr| {
                        if card.upgrade_level > 0 {
                            pr.upg_block
                        } else {
                            pr.base_block
                        }
                    })
                    .clamp(0, 24) as i64;
                if blk > 0 {
                    retained_hold_bonus +=
                        (90_000 - retained_hold_bonus.min(90_000)).min(blk * 1_500);
                }
            }
            let prof_setup_w = prof.map_or(0, |pr| i32::from(pr.setup_weight));
            if meta.card_type == CardType::Power || prof_setup_w > 0 {
                let hold_w = if prof_setup_w > 0 { prof_setup_w } else { 10 }.clamp(6, 24) as i64;
                let add = hold_w * 1_000;
                let room = (60_000 - retained_hold_bonus).max(0);
                retained_hold_bonus += room.min(add);
            }
        }

        if card.id == CardId::SovereignBlade {
            let blade_dmg = read_sovereign_blade_damage(state, card);
            let forged_delta = (blade_dmg - 10).max(0);
            total_forged_delta += forged_delta;
            let per_forged = if is_retained { 4_500i64 } else { 3_000i64 };
            let general_bonus = (i64::from(forged_delta) * per_forged * blade_synergy_mult) / 100;
            let mut furnace_hold_std = 0i64;
            let mut furnace_hold_surv = 0i64;
            if is_retained
                && p_acc.furnace_power > 0
                && fast.enemy_hp > blade_dmg.saturating_mul(2)
            {
                let clamped = i64::from((blade_dmg + p_acc.furnace_power).min(70));
                retained_blade_bonus_base += clamped;
                furnace_hold_std = clamped * 4_800;
                furnace_hold_surv = clamped * 6_500;
            }
            forged_blade_bonus_std += general_bonus.max(furnace_hold_std);
            forged_blade_bonus_surv += general_bonus.max(furnace_hold_surv);
        }

        let val = card_base_heuristic_value(card.id, card.upgrade_level, meta.card_type);
        reachable_hand_value += val;
        if meta.card_type == CardType::Attack {
            playable_attack_count += 1;
            retained_attack_value += val.max(1);
        } else if meta.card_type == CardType::Power {
            latent_setup_value += 2;
            if let Some(adm) = PowerCardValuationRegistry::admission(card.id) {
                if adm.priority > best_power_priority {
                    best_power_priority = adm.priority;
                }
            }
        }
        if meta.flags.contains(CardStaticFlags::HAS_GROWTH) {
            replay_potential_value += 1;
        }
        if is_retained {
            future_resource_value += val * 2;
        }

        let (playable, live_cost, star_cost, is_costs_x, is_star_costs_x) =
            card_is_playable_in_hand(state, card, meta, p_acc.has_player_corruption);
        if playable {
            playable_card_count += 1;
            if matches!(
                card.id,
                CardId::Prolong | CardId::Entrench | CardId::BodySlam
            ) {
                has_playable_block_scaler = true;
            }
            if live_cost == 0 && star_cost == 0 {
                zero_cost_playable_count += 1;
                if !is_costs_x
                    && !is_star_costs_x
                    && meta.canonical_energy_cost > 0
                    && meta.rarity != CardRarity::Basic
                {
                    let bonus = i64::from(meta.canonical_energy_cost.min(3)) * 24_000;
                    let room = 160_000 - cost_zeroed_hand_bonus;
                    if room > 0 {
                        cost_zeroed_hand_bonus += room.min(bonus);
                    }
                }
            }
        }
    }

    let extra_retained_cap = if has_copy_or_power_in_hand { 4 } else { 2 };
    retained_cards += extra_retained_candidates.min(extra_retained_cap);

    let effective_defense = if p_acc.preserves_excess_block {
        end_turn_block
    } else if has_playable_block_scaler {
        end_turn_block.min(e_acc.incoming_damage.max(40))
    } else {
        end_turn_block.min(e_acc.incoming_damage)
    } + pet_defense_buffer;

    let excess_current_block_scaler = if has_playable_block_scaler {
        (fast.player_block - e_acc.incoming_damage).clamp(0, 40)
    } else {
        0
    };

    if p_acc.no_draw_setup_penalty < 0
        && playable_card_count >= 4
        && zero_cost_playable_count >= 3
    {
        persistent_buff_value -= p_acc.no_draw_setup_penalty;
    }

    let mut max_undrawn_setup_weight = 0i32;
    for pile in [&state.player.draw[..], &state.player.discard[..]] {
        for &ch in pile {
            let Some(card) = state.card(ch) else {
                continue;
            };
            if card.id == CardId::Anger
                && (card.clone_of_handle.is_some()
                    || card.temp_flags.contains(CardTempFlags::IS_DUPE))
            {
                anger_copies_generated += 1;
            }
            let meta_opt = match card_metas.and_then(|cm| cm.get(card.id as usize)) {
                Some(m) => Some(m),
                None => content.card_meta(card.id).ok(),
            };
            let Some(meta) = meta_opt else {
                continue;
            };
            if matches!(meta.card_type, CardType::Status | CardType::Curse) {
                continue;
            }
            if track_ongoing_growth {
                if let Some(gb) = tracked_fatal_growth_budget(card.id, root) {
                    active_growth_deck_budget =
                        Some(active_growth_deck_budget.map_or(gb, |cur| cur.max(gb)));
                }
            }
            if card.id == CardId::SovereignBlade {
                let blade_dmg = read_sovereign_blade_damage(state, card);
                let forged_delta = (blade_dmg - 10).max(0);
                total_forged_delta += forged_delta;
                if forged_delta > 0 {
                    let b = (i64::from(forged_delta) * 3_000 * blade_synergy_mult) / 100;
                    forged_blade_bonus_std += b;
                    forged_blade_bonus_surv += b;
                }
            }
            if p_acc.player_retain_hand > 0 && has_nightmare_in_hand {
                let prof = card_rollout_profiles.and_then(|t| t.get(card.id as usize));
                let mut cand_w = prof.map_or(0, |pr| i32::from(pr.setup_weight));
                if meta.card_type == CardType::Power && cand_w < 18 {
                    cand_w = 18;
                }
                if card.upgrade_level > 0 && cand_w > 0 {
                    cand_w += 5;
                }
                if cand_w > max_undrawn_setup_weight {
                    max_undrawn_setup_weight = cand_w;
                }
            }
        }
    }

    if p_acc.player_retain_hand > 0 && has_nightmare_in_hand && max_undrawn_setup_weight >= 20 {
        retained_hold_bonus += i64::from(max_undrawn_setup_weight.min(50)) * 12_000;
        persistent_buff_value += (max_undrawn_setup_weight / 3).min(16);
    }

    let mut good_thinned_cards = 0i32;
    let mut wasted_engine_exhausts = 0i32;
    let mut exhausted_the_hunts = 0i32;
    for &ch in &state.player.exhaust {
        let Some(card) = state.card(ch) else {
            continue;
        };
        if card.id == CardId::Anger
            && (card.clone_of_handle.is_some() || card.temp_flags.contains(CardTempFlags::IS_DUPE))
        {
            anger_copies_generated += 1;
        }
        if !root.ignore_long_term_rewards
            && root.effective_has_growth_targets
            && card.id == CardId::TheHunt
        {
            exhausted_the_hunts += 1;
        }
        let meta_opt = match card_metas.and_then(|cm| cm.get(card.id as usize)) {
            Some(m) => Some(m),
            None => content.card_meta(card.id).ok(),
        };
        let Some(meta) = meta_opt else {
            continue;
        };
        if meta.rarity == CardRarity::Token {
            continue;
        }
        let kw = card_effective_keywords(card);
        let has_exhaust_kw = kw.contains(CardKeywords::EXHAUST);
        let is_good_thin = matches!(meta.card_type, CardType::Status | CardType::Curse)
            || (meta.rarity == CardRarity::Basic
                && (meta.base_tags.contains(CardTags::STRIKE)
                    || meta.base_tags.contains(CardTags::DEFEND)));
        let is_clone = card.clone_of_handle.is_some() || card.temp_flags.contains(CardTempFlags::IS_DUPE);
        if is_good_thin {
            good_thinned_cards += 1;
        } else if !has_exhaust_kw
            && !is_clone
            && (!p_acc.has_player_corruption || meta.card_type != CardType::Skill)
        {
            wasted_engine_exhausts += 1;
        }
    }
    if wasted_engine_exhausts > 0 && paels_eye_triggered {
        wasted_engine_exhausts = (wasted_engine_exhausts - 10).max(0);
    }
    let missed_the_hunt_rewards = (exhausted_the_hunts - p_acc.rewarded_the_hunts).max(0);

    let ready_arrows = if state.player.arrows.is_empty() {
        0
    } else {
        state
            .player
            .arrows
            .iter()
            .filter(|a| a.is_available())
            .count() as i32
    };
    let melody_cards_finished = if state.player.arrows.is_empty() {
        0
    } else {
        state.player.counters.melody_cards_finished_this_turn.max(0)
    };

    let all_reattach_encounter = e_acc.reattach_alive_count > 0
        && (e_acc.reattach_alive_count + fast.reviving_enemy_count) >= fast.alive_enemy_count;

    let setup_score = (persistent_buff_value + e_acc.enemy_debuff_value
        - e_acc.enemy_threat_power_penalty)
        .clamp(-200, 2000);
    let raw_thin = (root.initial_live_deck_size - fast.live_deck_size).max(0);
    let net_good_thinned = (raw_thin.min(good_thinned_cards) - wasted_engine_exhausts).max(0);
    let thin_deck_delta = ((net_good_thinned + 1) >> 1).min(6);
    let sandpit_board_investment = if e_acc.min_sandpit_turns < i32::MAX && fast.enemy_hp > 40 {
        e_acc.min_sandpit_turns.min(6) * 3
    } else {
        0
    };
    let stars = fast.stars.max(0);
    let board_investment = p_acc.player_buff_stacks
        + e_acc.enemy_debuff_stacks
        + state.player.orbs.len() as i32
        + orb_capacity
        + (dark_evoke_sum / 6)
        + (osty_hp / 4)
        + (retained_cards * 3)
        + thin_deck_delta
        + melody_cards_finished
        + sandpit_board_investment
        + (total_forged_delta / 3).min(20)
        + (stars / 2).min(5);

    let optionality_score = (fast.energy.max(0) + stars + playable_card_count + p_acc.next_turn_draw)
        .clamp(0, i16::MAX as i32) as i16 as u16;
    let playable_hand_u8 = playable_card_count.clamp(0, u8::MAX as i32) as u8;
    let mut potion_presence_mask = 0u16;
    let mut potion_filled_count = 0u8;
    let mut potion_id_mix = 0u8;
    for (slot_idx, slot) in state.player.potion_slots.iter().enumerate().take(16) {
        if let Some(pid) = *slot {
            potion_presence_mask |= 1u16 << slot_idx;
            potion_filled_count = potion_filled_count.saturating_add(1);
            potion_id_mix = potion_id_mix.rotate_left(3) ^ (pid as u8).wrapping_add(1);
        }
    }
    let player_max_hp_delta = (root.initial_player_max_hp - fast.player_max_hp)
        .clamp(i16::MIN as i32, i16::MAX as i32) as i16 as u16;
    let eff_def_u16 = effective_defense.clamp(0, i16::MAX as i32) as i16 as u16;
    let w0 = ((state.player.turn_number as u8) as u64)
        | (u64::from(potion_presence_mask) << 8)
        | (u64::from(potion_filled_count ^ potion_id_mix) << 24)
        | (u64::from(fast.death_save_use_count.clamp(0, 255) as u8) << 32)
        | (u64::from(fast.alive_enemy_count.clamp(0, 255) as u8) << 40)
        | (u64::from(fast.reviving_enemy_count.clamp(0, 255) as u8) << 48)
        | (u64::from((fast.energy as i16) as u8) << 56);
    let w1 = u64::from(fast.cumulative_hp_lost as u32)
        | (u64::from(fast.recovered_hp as u32) << 32);
    let w2 = u64::from(fast.player_hp as u32) | (u64::from(fast.enemy_hp as u32) << 32);
    let w3 = u64::from(player_max_hp_delta)
        | (u64::from(eff_def_u16) << 16)
        | (u64::from(optionality_score) << 32)
        | (u64::from(playable_hand_u8) << 48);
    let w4 = u64::from(setup_score as u32) | (u64::from(board_investment as u32) << 32);
    let w5 = u64::from(max_dark_evoke as u32) | (u64::from(e_acc.total_poison_and_doom as u32) << 32);

    let mut sig = 0xD1B5_4A32_D192_ED03u64;
    sig = splitmix64(sig ^ w0);
    sig = splitmix64(sig ^ w1);
    sig = splitmix64(sig ^ w2);
    sig = splitmix64(sig ^ w3);
    sig = splitmix64(sig ^ w4);
    sig = splitmix64(sig ^ w5);
    if root.theft_policy == SolverTheftPolicy::PreserveResources {
        let w_theft = u64::from(fast.outstanding_stolen.clamp(i16::MIN as i32, i16::MAX as i32) as i16 as u16)
            | if fast.forfeits_gold { 1u64 << 16 } else { 0 };
        sig = splitmix64(sig ^ w_theft);
    }
    if !root.ignore_long_term_rewards && root.effective_has_growth_targets {
        let w_growth = u64::from(fast.growth_hp_credit.clamp(i16::MIN as i32, i16::MAX as i32) as i16 as u16)
            | (u64::from(fast.growth_reward_count.clamp(0, u16::MAX as i32) as u16) << 16)
            | (u64::from(fast.alchemize_count) << 32)
            | (u64::from(fast.feed_count) << 40);
        sig = splitmix64(sig ^ w_growth);
    }
    if !root.active_relic_targets.is_empty() {
        let w_relic = u64::from(fast.relic_eval.satisfied_mask as u16)
            | (u64::from(fast.relic_eval.distance.clamp(0, u16::MAX as i32) as u16) << 16)
            | (u64::from(fast.relic_eval.satisfied_priority.clamp(0, u16::MAX as i32) as u16) << 32)
            | (u64::from(fast.relic_eval.hp_credit.clamp(i16::MIN as i32, i16::MAX as i32) as i16 as u16) << 48);
        sig = splitmix64(sig ^ w_relic);
    }
    let tactical_sig = if sig == 0 { 1 } else { sig };

    FullEvalDetails {
        persistent_buff_value,
        setup_score,
        expected_regen_heal: p_acc.expected_regen_heal,
        board_investment,
        enemy_debuff_value: e_acc.enemy_debuff_value,
        enemy_threat_power_penalty: e_acc.enemy_threat_power_penalty,
        orb_score,
        orb_capacity,
        max_dark_evoke,
        total_poison_and_doom: e_acc.total_poison_and_doom,
        doom_lethal_count: e_acc.doom_lethal_count,
        effective_doom_value: e_acc.effective_doom_value,
        poison_lethal_count: e_acc.poison_lethal_count,
        poison_lethal_hp_offset: e_acc.poison_lethal_hp_offset,
        poison_hp_normal: e_acc.poison_hp_normal,
        poison_hp_reviving: e_acc.poison_hp_reviving,
        next_turn_stars: p_acc.next_turn_stars,
        next_turn_summon: p_acc.next_turn_summon,
        latent_setup_value,
        replay_potential_value,
        retained_attack_value,
        reachable_hand_value,
        future_resource_value,
        delayed_damage_value: e_acc.delayed_damage_value,
        reactive_damage_value: 0,
        enemy_strength_suppression: e_acc.enemy_strength_suppression,
        enemy_weak_turns: e_acc.enemy_weak_turns,
        enemy_vulnerable_turns: e_acc.enemy_vulnerable_turns,
        focus_target_pressure: (root.initial_enemy_count as i32 * 100 - fast.enemy_hp).max(0),
        focus_target_remaining_hp: e_acc.focus_target_remaining_hp,
        osty_hp,
        osty_max_hp,
        osty_absorbed: e_acc.osty_absorbed,
        wuppy_hp,
        min_sandpit_turns: e_acc.min_sandpit_turns,
        furnace_power: p_acc.furnace_power,
        retained_blade_bonus_base,
        forged_blade_bonus_std,
        forged_blade_bonus_surv,
        retained_hold_bonus,
        cost_zeroed_hand_bonus,
        excess_current_block_scaler,
        ready_arrows,
        melody_cards_finished,
        playable_attack_count,
        incoming_damage: e_acc.incoming_damage,
        end_turn_block,
        effective_defense,
        unblocked_incoming: e_acc.unblocked_incoming,
        remaining_block: e_acc.remaining_block,
        paper_cuts_max_hp_threat: e_acc.paper_cuts_max_hp_threat,
        enrage_phase_threat_dmg: e_acc.enrage_phase_threat_dmg,
        player_intangible: p_acc.player_intangible,
        paels_legion_ready,
        reviving_hp_penalty: e_acc.reviving_hp_penalty,
        all_reattach_encounter,
        highest_alive_enemy_hp: e_acc.highest_alive_enemy_hp,
        preserves_excess_block: p_acc.preserves_excess_block,
        next_turn_block: p_acc.next_turn_block,
        next_turn_energy: p_acc.next_turn_energy,
        next_turn_draw: p_acc.next_turn_draw,
        playable_card_count,
        zero_cost_playable_count,
        hand_clutter,
        wasted_engine_exhausts,
        missed_the_hunt_rewards,
        alive_non_minion_enemies: e_acc.alive_non_minion_enemies,
        active_growth_hand_budget,
        active_growth_deck_budget,
        anger_copies_generated,
        enemy_combat_dist_hash: 0,
        unordered_pile_hash: 0,
        tactical_sig,
        best_power_priority,
        beam_rank_score: 0.0,
        rollout_score: None,
        choice_lookahead_score: None,
        has_choice_lookahead: false,
    }
}

pub fn evaluate_full_details(
    state: &BattleState,
    content: &dyn ContentDispatcher,
    fast: &FastEvalSummary,
    root: &RootEvalBaseline,
) -> FullEvalDetails {
    evaluate_full_details_cached(state, content, fast, root, None)
}

pub(crate) fn evaluate_full_details_cached(
    state: &BattleState,
    content: &dyn ContentDispatcher,
    fast: &FastEvalSummary,
    root: &RootEvalBaseline,
    intent_cache: Option<&mut IntentHitsCache>,
) -> FullEvalDetails {
    let eval_tables = state.hook_cache.cached_tables;
    let power_metas = eval_tables.map(|t| t.power_metas);
    let card_metas = eval_tables.map(|t| t.card_metas);
    let card_rollout_profiles = eval_tables
        .map(|t| t.card_rollout_profiles)
        .or_else(|| content.hook_mask_tables().map(|t| t.card_rollout_profiles));
    let pre = eval_player_pre_enemy_state(
        state,
        content,
        root,
        power_metas,
        card_metas,
        card_rollout_profiles,
        fast.player_hp,
        fast.player_max_hp,
        fast.player_block,
    );

    let mut dummy_fast = EnemyFastEvalAcc::default();
    let mut e_acc = EnemyFullEvalAcc::from_pre_enemy(&pre);
    eval_enemies_pass::<false, true>(
        state,
        content,
        root,
        power_metas,
        pre.p_acc.player_accelerant,
        pre.p_acc.player_vuln,
        pre.p_acc.player_intangible,
        intent_cache,
        &mut dummy_fast,
        &mut e_acc,
    );

    eval_finalize_full_details(
        state,
        content,
        fast,
        root,
        card_metas,
        card_rollout_profiles,
        pre,
        e_acc,
    )
}

pub fn evaluate_full(
    state: &BattleState,
    content: &dyn ContentDispatcher,
    fast: &FastEvalSummary,
    root: &RootEvalBaseline,
) -> FullEvalDetails {
    let mut details = evaluate_full_details(state, content, fast, root);
    details.beam_rank_score = compute_beam_rank_score(
        fast,
        &details,
        root,
        false,
        false,
        None,
    );
    details
}

#[cfg(any(debug_assertions, test))]
fn debug_assert_fused_parity(
    expected_fast: &FastEvalSummary,
    actual_fast: &FastEvalSummary,
    expected_full: &FullEvalDetails,
    actual_full: &FullEvalDetails,
) {
    assert_eq!(actual_fast.player_hp, expected_fast.player_hp);
    assert_eq!(actual_fast.player_max_hp, expected_fast.player_max_hp);
    assert_eq!(actual_fast.player_block, expected_fast.player_block);
    assert_eq!(actual_fast.projected_player_hp, expected_fast.projected_player_hp);
    assert_eq!(actual_fast.cumulative_hp_lost, expected_fast.cumulative_hp_lost);
    assert_eq!(actual_fast.recovered_hp, expected_fast.recovered_hp);
    assert_eq!(actual_fast.enemy_hp, expected_fast.enemy_hp);
    assert_eq!(actual_fast.raw_enemy_hp, expected_fast.raw_enemy_hp);
    assert_eq!(actual_fast.enemy_block, expected_fast.enemy_block);
    assert_eq!(actual_fast.alive_enemy_count, expected_fast.alive_enemy_count);
    assert_eq!(actual_fast.reviving_enemy_count, expected_fast.reviving_enemy_count);
    assert_eq!(actual_fast.alive_enemy_mask, expected_fast.alive_enemy_mask);
    assert_eq!(actual_fast.player_dead, expected_fast.player_dead);
    assert_eq!(actual_fast.all_enemies_dead, expected_fast.all_enemies_dead);
    assert_eq!(actual_fast.won, expected_fast.won);
    assert_eq!(actual_fast.is_event_defeat, expected_fast.is_event_defeat);
    assert_eq!(actual_fast.has_pending_choice, expected_fast.has_pending_choice);
    assert_eq!(actual_fast.death_save_use_count, expected_fast.death_save_use_count);
    assert_eq!(actual_fast.death_save_hp_restored, expected_fast.death_save_hp_restored);
    assert_eq!(actual_fast.growth_hp_credit, expected_fast.growth_hp_credit);
    assert_eq!(actual_fast.growth_reward_count, expected_fast.growth_reward_count);
    assert_eq!(actual_fast.raw_growth_reward_count, expected_fast.raw_growth_reward_count);
    assert_eq!(actual_fast.growth_rewards, expected_fast.growth_rewards);
    assert_eq!(actual_fast.relic_eval, expected_fast.relic_eval);
    assert_eq!(actual_fast.strategy_goal_hp_credit, expected_fast.strategy_goal_hp_credit);
    assert_eq!(actual_fast.strategic_hp_credit, expected_fast.strategic_hp_credit);
    assert_eq!(actual_fast.strategy_goal_count, expected_fast.strategy_goal_count);
    assert_eq!(
        actual_fast.all_strategy_targets_satisfied,
        expected_fast.all_strategy_targets_satisfied
    );
    assert_eq!(
        actual_fast.brightest_flame_max_hp_spent,
        expected_fast.brightest_flame_max_hp_spent
    );
    assert_eq!(actual_fast.energy, expected_fast.energy);
    assert_eq!(actual_fast.stars, expected_fast.stars);
    assert_eq!(actual_fast.hand_count, expected_fast.hand_count);
    assert_eq!(actual_fast.playable_hand_count, expected_fast.playable_hand_count);
    assert_eq!(actual_fast.live_deck_size, expected_fast.live_deck_size);
    assert_eq!(actual_fast.live_deck_clutter, expected_fast.live_deck_clutter);
    assert_eq!(actual_fast.alchemize_count, expected_fast.alchemize_count);
    assert_eq!(actual_fast.empty_potion_slots, expected_fast.empty_potion_slots);
    assert_eq!(actual_fast.feed_count, expected_fast.feed_count);
    assert_eq!(actual_fast.outstanding_stolen, expected_fast.outstanding_stolen);
    assert_eq!(actual_fast.forfeits_gold, expected_fast.forfeits_gold);
    assert_eq!(actual_fast.combat_ended_turn, expected_fast.combat_ended_turn);
    assert_eq!(actual_fast.base_score.to_bits(), expected_fast.base_score.to_bits());

    assert_eq!(actual_full.persistent_buff_value, expected_full.persistent_buff_value);
    assert_eq!(actual_full.setup_score, expected_full.setup_score);
    assert_eq!(actual_full.expected_regen_heal, expected_full.expected_regen_heal);
    assert_eq!(actual_full.board_investment, expected_full.board_investment);
    assert_eq!(actual_full.enemy_debuff_value, expected_full.enemy_debuff_value);
    assert_eq!(
        actual_full.enemy_threat_power_penalty,
        expected_full.enemy_threat_power_penalty
    );
    assert_eq!(actual_full.orb_score, expected_full.orb_score);
    assert_eq!(actual_full.orb_capacity, expected_full.orb_capacity);
    assert_eq!(actual_full.max_dark_evoke, expected_full.max_dark_evoke);
    assert_eq!(actual_full.total_poison_and_doom, expected_full.total_poison_and_doom);
    assert_eq!(actual_full.doom_lethal_count, expected_full.doom_lethal_count);
    assert_eq!(actual_full.effective_doom_value, expected_full.effective_doom_value);
    assert_eq!(actual_full.poison_lethal_count, expected_full.poison_lethal_count);
    assert_eq!(actual_full.poison_lethal_hp_offset, expected_full.poison_lethal_hp_offset);
    assert_eq!(actual_full.poison_hp_normal, expected_full.poison_hp_normal);
    assert_eq!(actual_full.poison_hp_reviving, expected_full.poison_hp_reviving);
    assert_eq!(actual_full.next_turn_stars, expected_full.next_turn_stars);
    assert_eq!(actual_full.next_turn_summon, expected_full.next_turn_summon);
    assert_eq!(actual_full.latent_setup_value, expected_full.latent_setup_value);
    assert_eq!(actual_full.replay_potential_value, expected_full.replay_potential_value);
    assert_eq!(actual_full.retained_attack_value, expected_full.retained_attack_value);
    assert_eq!(actual_full.reachable_hand_value, expected_full.reachable_hand_value);
    assert_eq!(actual_full.future_resource_value, expected_full.future_resource_value);
    assert_eq!(actual_full.delayed_damage_value, expected_full.delayed_damage_value);
    assert_eq!(actual_full.reactive_damage_value, expected_full.reactive_damage_value);
    assert_eq!(
        actual_full.enemy_strength_suppression,
        expected_full.enemy_strength_suppression
    );
    assert_eq!(actual_full.enemy_weak_turns, expected_full.enemy_weak_turns);
    assert_eq!(
        actual_full.enemy_vulnerable_turns,
        expected_full.enemy_vulnerable_turns
    );
    assert_eq!(actual_full.focus_target_pressure, expected_full.focus_target_pressure);
    assert_eq!(
        actual_full.focus_target_remaining_hp,
        expected_full.focus_target_remaining_hp
    );
    assert_eq!(actual_full.osty_hp, expected_full.osty_hp);
    assert_eq!(actual_full.osty_max_hp, expected_full.osty_max_hp);
    assert_eq!(actual_full.osty_absorbed, expected_full.osty_absorbed);
    assert_eq!(actual_full.wuppy_hp, expected_full.wuppy_hp);
    assert_eq!(actual_full.min_sandpit_turns, expected_full.min_sandpit_turns);
    assert_eq!(actual_full.furnace_power, expected_full.furnace_power);
    assert_eq!(
        actual_full.retained_blade_bonus_base,
        expected_full.retained_blade_bonus_base
    );
    assert_eq!(actual_full.forged_blade_bonus_std, expected_full.forged_blade_bonus_std);
    assert_eq!(actual_full.forged_blade_bonus_surv, expected_full.forged_blade_bonus_surv);
    assert_eq!(actual_full.retained_hold_bonus, expected_full.retained_hold_bonus);
    assert_eq!(actual_full.cost_zeroed_hand_bonus, expected_full.cost_zeroed_hand_bonus);
    assert_eq!(
        actual_full.excess_current_block_scaler,
        expected_full.excess_current_block_scaler
    );
    assert_eq!(actual_full.ready_arrows, expected_full.ready_arrows);
    assert_eq!(actual_full.melody_cards_finished, expected_full.melody_cards_finished);
    assert_eq!(actual_full.playable_attack_count, expected_full.playable_attack_count);
    assert_eq!(actual_full.incoming_damage, expected_full.incoming_damage);
    assert_eq!(actual_full.end_turn_block, expected_full.end_turn_block);
    assert_eq!(actual_full.effective_defense, expected_full.effective_defense);
    assert_eq!(actual_full.unblocked_incoming, expected_full.unblocked_incoming);
    assert_eq!(actual_full.remaining_block, expected_full.remaining_block);
    assert_eq!(
        actual_full.paper_cuts_max_hp_threat,
        expected_full.paper_cuts_max_hp_threat
    );
    assert_eq!(
        actual_full.enrage_phase_threat_dmg,
        expected_full.enrage_phase_threat_dmg
    );
    assert_eq!(actual_full.player_intangible, expected_full.player_intangible);
    assert_eq!(actual_full.paels_legion_ready, expected_full.paels_legion_ready);
    assert_eq!(actual_full.reviving_hp_penalty, expected_full.reviving_hp_penalty);
    assert_eq!(actual_full.all_reattach_encounter, expected_full.all_reattach_encounter);
    assert_eq!(actual_full.highest_alive_enemy_hp, expected_full.highest_alive_enemy_hp);
    assert_eq!(actual_full.preserves_excess_block, expected_full.preserves_excess_block);
    assert_eq!(actual_full.next_turn_block, expected_full.next_turn_block);
    assert_eq!(actual_full.next_turn_energy, expected_full.next_turn_energy);
    assert_eq!(actual_full.next_turn_draw, expected_full.next_turn_draw);
    assert_eq!(actual_full.playable_card_count, expected_full.playable_card_count);
    assert_eq!(
        actual_full.zero_cost_playable_count,
        expected_full.zero_cost_playable_count
    );
    assert_eq!(actual_full.hand_clutter, expected_full.hand_clutter);
    assert_eq!(actual_full.wasted_engine_exhausts, expected_full.wasted_engine_exhausts);
    assert_eq!(actual_full.missed_the_hunt_rewards, expected_full.missed_the_hunt_rewards);
    assert_eq!(
        actual_full.alive_non_minion_enemies,
        expected_full.alive_non_minion_enemies
    );
    assert_eq!(
        actual_full.active_growth_hand_budget,
        expected_full.active_growth_hand_budget
    );
    assert_eq!(
        actual_full.active_growth_deck_budget,
        expected_full.active_growth_deck_budget
    );
    assert_eq!(actual_full.anger_copies_generated, expected_full.anger_copies_generated);
    assert_eq!(
        actual_full.enemy_combat_dist_hash,
        expected_full.enemy_combat_dist_hash
    );
    assert_eq!(actual_full.unordered_pile_hash, expected_full.unordered_pile_hash);
    assert_eq!(actual_full.tactical_sig, expected_full.tactical_sig);
    assert_eq!(actual_full.best_power_priority, expected_full.best_power_priority);
    assert_eq!(
        actual_full.beam_rank_score.to_bits(),
        expected_full.beam_rank_score.to_bits()
    );
    assert_eq!(
        actual_full.rollout_score.map(f64::to_bits),
        expected_full.rollout_score.map(f64::to_bits)
    );
    assert_eq!(
        actual_full.choice_lookahead_score.map(f64::to_bits),
        expected_full.choice_lookahead_score.map(f64::to_bits)
    );
    assert_eq!(actual_full.has_choice_lookahead, expected_full.has_choice_lookahead);
}

/// Fused single-pass evaluation computing both `FastEvalSummary` and `FullEvalDetails` without scanning creatures twice.
pub fn evaluate_fast_and_full_fused(
    state: &BattleState,
    content: &dyn ContentDispatcher,
    root: &RootEvalBaseline,
    _action_count: u16,
    _potion_count: u8,
    stand_pat_hp: Option<i32>,
    intent_cache: Option<&mut IntentHitsCache>,
) -> (FastEvalSummary, FullEvalDetails, Option<RareNodeEval>) {
    let pc = state.player_creature();
    let player_hp = pc.map_or(state.player.current_hp, |c| c.current_hp);
    let player_max_hp = pc.map_or(state.player.max_hp, |c| c.max_hp);
    let player_block = pc.map_or(state.player.block, |c| c.block);
    let cumulative_hp_lost = pc.map_or(state.player.cumulative_hp_lost, |c| c.cumulative_hp_lost);
    let pc_dead = pc.is_none_or(|c| c.is_dead());

    let eval_tables = state.hook_cache.cached_tables;
    let power_metas = eval_tables.map(|t| t.power_metas);
    let card_metas = eval_tables.map(|t| t.card_metas);
    let card_rollout_profiles = eval_tables
        .map(|t| t.card_rollout_profiles)
        .or_else(|| content.hook_mask_tables().map(|t| t.card_rollout_profiles));
    let pre = eval_player_pre_enemy_state(
        state,
        content,
        root,
        power_metas,
        card_metas,
        card_rollout_profiles,
        player_hp,
        player_max_hp,
        player_block,
    );

    let mut fast_acc = EnemyFastEvalAcc::default();
    let mut e_acc = EnemyFullEvalAcc::from_pre_enemy(&pre);
    eval_enemies_pass::<true, true>(
        state,
        content,
        root,
        power_metas,
        pre.p_acc.player_accelerant,
        pre.p_acc.player_vuln,
        pre.p_acc.player_intangible,
        intent_cache,
        &mut fast_acc,
        &mut e_acc,
    );

    let fast = eval_build_fast_summary(
        state,
        content,
        root,
        stand_pat_hp,
        pc_dead,
        player_hp,
        player_max_hp,
        player_block,
        cumulative_hp_lost,
        fast_acc,
    );
    let full = eval_finalize_full_details(
        state,
        content,
        &fast,
        root,
        card_metas,
        card_rollout_profiles,
        pre,
        e_acc,
    );
    let rare = if fast.growth_rewards.is_zero() && fast.relic_eval.target_mask == 0 {
        None
    } else {
        Some(RareNodeEval {
            growth_rewards: fast.growth_rewards,
            relic_eval: fast.relic_eval,
        })
    };

    #[cfg(debug_assertions)]
    {
        let expected_fast = evaluate_fast(state, content, root, stand_pat_hp);
        let expected_full = evaluate_full_details_cached(state, content, &expected_fast, root, None);
        debug_assert_fused_parity(&expected_fast, &fast, &expected_full, &full);
    }

    (fast, full, rare)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BeamWeightTerm {
    CurrentEnergy,
    PersistentBuffDelta,
    EnemyHp,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BeamWeightPerturbation {
    pub term: BeamWeightTerm,
    pub scale: f64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum EvalMode {
    #[default]
    Balanced,
    SetupAndSurvival,
    Offense,
    DesperateRescue,
}

pub fn compute_beam_rank_score(
    fast: &FastEvalSummary,
    full: &FullEvalDetails,
    root: &RootEvalBaseline,
    base_score_only: bool,
    boss_tempo_hp_pricing: bool,
    perturbation: Option<BeamWeightPerturbation>,
) -> f64 {
    compute_beam_rank_score_with_mode(
        fast,
        full,
        root,
        base_score_only,
        boss_tempo_hp_pricing,
        perturbation,
        EvalMode::Balanced,
        0,
        0,
    )
}

#[inline]
fn soft_cap_positive(value: i32, knee: i32, second_knee: i32) -> i32 {
    if value <= knee {
        return value;
    }
    let first_band = knee + ((value - knee) >> 1);
    if first_band <= second_knee {
        return first_band;
    }
    let tail = second_knee + ((first_band - second_knee) >> 1);
    tail.min(10_000)
}

#[allow(clippy::too_many_arguments)]
pub fn compute_beam_rank_score_with_mode(
    fast: &FastEvalSummary,
    full: &FullEvalDetails,
    root: &RootEvalBaseline,
    base_score_only: bool,
    boss_tempo_hp_pricing: bool,
    perturbation: Option<BeamWeightPerturbation>,
    eval_mode: EvalMode,
    potion_strategic_cost: i32,
    action_count: u16,
) -> f64 {
    let net_potion_cost_centi = potion_strategic_cost
        .max(0)
        .saturating_mul(weights::HP_FIXED_SCALE);
    compute_beam_rank_score_with_centi(
        fast,
        full,
        root,
        base_score_only,
        boss_tempo_hp_pricing,
        perturbation,
        eval_mode,
        net_potion_cost_centi,
        action_count,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn compute_beam_rank_score_with_centi(
    fast: &FastEvalSummary,
    full: &FullEvalDetails,
    root: &RootEvalBaseline,
    base_score_only: bool,
    boss_tempo_hp_pricing: bool,
    perturbation: Option<BeamWeightPerturbation>,
    eval_mode: EvalMode,
    net_potion_cost_centi: i32,
    action_count: u16,
) -> f64 {
    if base_score_only || fast.player_dead || fast.projected_player_hp <= 0 {
        return fast.base_score;
    }
    if fast.is_event_defeat {
        return -500_000_000_000.0
            - fast.death_save_use_count as f64 * 500_000_000.0
            - fast.enemy_hp as f64 * 10_000.0
            - fast.cumulative_hp_lost as f64 * 50_000.0
            - net_potion_cost_centi as f64 * 390.0;
    }
    if fast.won {
        let relic_heal = ActEndingBossPolicy::ranked_post_combat_relic_heal(
            root.post_combat_relic_heal,
            true,
            fast.player_hp,
            fast.player_max_hp,
        );
        let net_hp_delta_centi = ActEndingBossPolicy::compute_net_persistent_hp_delta_centi(
            fast.player_hp,
            relic_heal,
            root.initial_player_hp,
            fast.player_max_hp,
            root.initial_player_max_hp,
            fast.death_save_hp_restored,
            fast.strategic_hp_credit,
            root.boss_hp_relief,
            root.ignore_long_term_rewards,
            i32::from(fast.feed_count) * root.feed_max_hp_per_kill,
        );
        let v_centi = i64::from(net_hp_delta_centi) - i64::from(net_potion_cost_centi);
        let mut s = weights::VICTORY_BONUS
            - fast.death_save_use_count as f64 * 500_000_000.0
            + v_centi as f64 * 390.0
            + net_hp_delta_centi as f64 * 2.0
            - fast.combat_ended_turn.unwrap_or(root.start_turn_number) as f64 * 5_000.0
            - action_count as f64 * 10.0;
        if !root.ignore_long_term_rewards && fast.growth_reward_count > 0 {
            s += fast.growth_reward_count as f64
                * if root.effective_has_growth_targets {
                    25_000.0
                } else {
                    5_000.0
                };
        }
        if !root.active_relic_targets.is_empty() {
            s += compute_relic_guidance_score(
                fast.relic_eval.satisfied_priority,
                fast.relic_eval.distance,
            ) as f64;
        }
        if root.theft_policy == SolverTheftPolicy::PreserveResources {
            s -= fast.outstanding_stolen as f64 * 1_000_000.0;
            if fast.forfeits_gold {
                s -= 2_000_000.0;
            }
        }
        return s;
    }

    let _ = (
        boss_tempo_hp_pricing,
        root.is_act_ending_boss,
        root.initial_persistent_buff_value,
        root.initial_retained_attack_value,
        root.initial_enemy_strength_suppression,
        root.initial_enemy_weak_turns,
    );

    let (
        hp_weight_per_centi,
        enemy_hp_weight,
        threat_weight,
        setup_scale,
        carryover_mult,
        death_save_penalty,
    ) = match eval_mode {
        EvalMode::SetupAndSurvival => (420i64, 9_500i64, 36_000i64, 21_500i64, 125i64, 500_000_000i64),
        EvalMode::Offense => (300, 13_500, 25_000, 15_000, 100, 1_500_000),
        EvalMode::DesperateRescue => (240, 10_000, 24_000, 30_000, 135, 1_500_000),
        EvalMode::Balanced => (390, 10_000, 34_000, 16_500, 100, 500_000_000),
    };

    let (ongoing_hp_weight, non_lethal_threat_weight) =
        if root.boss_hp_relief != BossHpRelief::None {
            match eval_mode {
                EvalMode::Offense => {
                    ((hp_weight_per_centi * 55) / 100, (threat_weight * 55) / 100)
                }
                EvalMode::DesperateRescue => {
                    ((hp_weight_per_centi * 40) / 100, (threat_weight * 40) / 100)
                }
                _ => (hp_weight_per_centi, threat_weight),
            }
        } else {
            (hp_weight_per_centi, threat_weight)
        };

    // Ongoing survival runway uses un-discounted BossHpRelief::None; only Offense/DesperateRescue
    // apply controlled non-lethal BossHpRelief scaling while preserving lethal guards at 100%.
    let ongoing_net_hp_delta_centi = ActEndingBossPolicy::compute_net_persistent_hp_delta_centi(
        fast.player_hp,
        0,
        root.initial_player_hp,
        fast.player_max_hp,
        root.initial_player_max_hp,
        fast.death_save_hp_restored,
        fast.strategic_hp_credit,
        BossHpRelief::None,
        root.ignore_long_term_rewards,
        i32::from(fast.feed_count) * root.feed_max_hp_per_kill,
    );

    let mut running_score: i64 = i64::from(ongoing_net_hp_delta_centi) * ongoing_hp_weight
        - i64::from(net_potion_cost_centi) * hp_weight_per_centi
        - i64::from(fast.death_save_use_count) * death_save_penalty;

    let lethal_incoming = full.unblocked_incoming >= fast.player_hp;
    let playable_cards = full
        .playable_card_count
        .max(i32::from(fast.playable_hand_count));

    if full.unblocked_incoming > 0 {
        running_score -= i64::from(full.unblocked_incoming)
            * if lethal_incoming {
                threat_weight
            } else {
                non_lethal_threat_weight
            };
        if lethal_incoming {
            running_score -= if playable_cards == 0 {
                4_000_000
            } else {
                800_000
            };
        }
    } else if full.preserves_excess_block {
        let rem_blk = if full.remaining_block > 0 {
            full.remaining_block
        } else {
            (full.end_turn_block - full.incoming_damage).max(0)
        };
        if rem_blk > 0 {
            running_score += i64::from(rem_blk.min(60)) * 20_000;
        }
    }

    if full.paels_legion_ready {
        running_score += 35_000;
    }

    if full.paper_cuts_max_hp_threat > 0 {
        let max_hp_loss_penalty_weight = if root.boss_hp_relief != BossHpRelief::RunEnding {
            26_000.min(threat_weight)
        } else {
            10_000
        };
        running_score -= i64::from(full.paper_cuts_max_hp_threat) * max_hp_loss_penalty_weight;
    }

    if full.enrage_phase_threat_dmg > 0 {
        let excess_carried = if full.preserves_excess_block {
            if full.remaining_block > 0 {
                full.remaining_block
            } else {
                (full.end_turn_block - full.incoming_damage).max(0)
            }
        } else {
            0
        };
        let intangible_shield = if full.player_intangible > 1 {
            full.enrage_phase_threat_dmg
        } else {
            0
        };
        let unbuffered_enrage = (full.enrage_phase_threat_dmg
            - excess_carried
            - full.next_turn_block.max(0)
            - intangible_shield)
            .max(0);
        if unbuffered_enrage > 0 {
            running_score -= i64::from(unbuffered_enrage) * (non_lethal_threat_weight / 3);
        }
    }

    if full.next_turn_block > 0 {
        running_score += (i64::from(full.next_turn_block.min(60)) * 20_000 * carryover_mult) / 100;
    }
    if full.next_turn_energy > 0 {
        running_score +=
            (i64::from(full.next_turn_energy.min(6)) * 34_000 * carryover_mult) / 100;
    }
    if full.next_turn_draw > 0 {
        running_score += (i64::from(full.next_turn_draw.min(6)) * 18_000 * carryover_mult) / 100;
    }
    if full.next_turn_stars > 0 {
        running_score += (i64::from(full.next_turn_stars.min(8)) * 20_000 * carryover_mult) / 100;
    }
    if full.next_turn_summon > 0 {
        running_score += i64::from(full.next_turn_summon.min(20)) * 6_000;
    }

    let remaining_non_dead = fast.alive_enemy_count + fast.reviving_enemy_count;
    let permanent_kills = (root.initial_enemy_count as i32 - remaining_non_dead).max(0)
        + full.doom_lethal_count
        + full.poison_lethal_count;
    running_score += i64::from(permanent_kills) * 110_000;
    running_score -= i64::from(fast.enemy_hp) * enemy_hp_weight;
    running_score -= i64::from(fast.enemy_block.max(0)) * 2_000;

    if full.poison_lethal_hp_offset > 0 {
        running_score +=
            i64::from(full.poison_lethal_hp_offset) * ((enemy_hp_weight * 95) / 100);
    }

    let poison_weight = 4_800i64.min((enemy_hp_weight * 48) / 100);
    if full.poison_hp_normal > 0 || full.poison_hp_reviving > 0 {
        running_score += full.poison_hp_normal * poison_weight
            + full.poison_hp_reviving * (poison_weight / 2);
    }

    if full.reviving_hp_penalty > 0 {
        running_score -= i64::from(full.reviving_hp_penalty) * ((enemy_hp_weight * 92) / 100);
    }

    if full.effective_doom_value > 0 {
        running_score += i64::from(full.effective_doom_value) * 5_500;
    }

    if full.all_reattach_encounter && fast.alive_enemy_count > 1 {
        running_score -= i64::from(full.highest_alive_enemy_hp.max(0)) * 2_500;
    } else if fast.alive_enemy_count > 1 && full.focus_target_remaining_hp < i32::MAX {
        let avg_hp = fast.enemy_hp / fast.alive_enemy_count;
        let focus_gap = (avg_hp - full.focus_target_remaining_hp).clamp(0, 60);
        running_score += i64::from(focus_gap) * 1_200;
    }

    if full.min_sandpit_turns < i32::MAX && fast.enemy_hp > 40 {
        let clamped_sandpit = full.min_sandpit_turns.min(6);
        running_score += i64::from(clamped_sandpit) * 60_000;
        if full.min_sandpit_turns <= 1 {
            running_score -= 1_200_000;
        } else if full.min_sandpit_turns == 2 {
            running_score -= 450_000;
        } else if full.min_sandpit_turns == 3 {
            running_score -= 150_000;
        }
    }

    if full.orb_capacity > 0 {
        running_score += i64::from(full.orb_capacity) * 18_000;
    }
    if full.orb_score > 0 {
        running_score += i64::from(soft_cap_positive(full.orb_score, 120, 360)) * 2_500;
    }
    let surviving_osty_hp = (full.osty_hp - full.osty_absorbed).max(0);
    if surviving_osty_hp > 0 {
        running_score += i64::from(soft_cap_positive(surviving_osty_hp, 80, 320)) * 4_500;
    }

    if full.ready_arrows > 0 {
        running_score += i64::from(full.ready_arrows) * 6_000;
    }
    if full.melody_cards_finished > 0 {
        running_score += i64::from(full.melody_cards_finished) * 5_000;
    }

    if full.excess_current_block_scaler > 0 {
        running_score += i64::from(full.excess_current_block_scaler) * 12_000;
    }

    running_score += i64::from(fast.energy.clamp(0, 6)) * 55_000;
    let stars = fast.stars.max(0);
    if stars > 0 {
        let first_six = stars.min(6);
        let next_six = (stars - 6).clamp(0, 6);
        running_score += i64::from(first_six) * 26_000 + i64::from(next_six) * 14_000;
    }

    let forged_blade_bonus = if full.forged_blade_bonus_std > 0 || full.forged_blade_bonus_surv > 0
    {
        if matches!(
            eval_mode,
            EvalMode::SetupAndSurvival | EvalMode::DesperateRescue
        ) {
            full.forged_blade_bonus_surv
        } else {
            full.forged_blade_bonus_std
        }
    } else if full.retained_blade_bonus_base > 0 {
        let per_dmg = if matches!(
            eval_mode,
            EvalMode::SetupAndSurvival | EvalMode::DesperateRescue
        ) {
            6_500i64
        } else {
            4_800i64
        };
        full.retained_blade_bonus_base * per_dmg
    } else {
        0
    };

    running_score += i64::from(playable_cards.clamp(0, 10)) * 18_000;
    running_score += i64::from(full.zero_cost_playable_count.clamp(0, 8)) * 11_000;
    running_score += full.cost_zeroed_hand_bonus;
    running_score += forged_blade_bonus + full.retained_hold_bonus;
    running_score -= i64::from(full.hand_clutter) * 4_000;
    running_score -= i64::from(fast.live_deck_clutter) * 4_500;
    running_score -= i64::from(full.wasted_engine_exhausts) * 20_000;

    let raw_total_setup =
        full.persistent_buff_value + full.enemy_debuff_value - full.enemy_threat_power_penalty;
    let total_setup = raw_total_setup.clamp(-200, 2000);
    let eff_setup = if total_setup >= 0 {
        soft_cap_positive(total_setup, 80, 240)
    } else {
        total_setup.max(-96)
    };
    running_score += i64::from(eff_setup) * setup_scale;

    if !root.ignore_long_term_rewards {
        if root.effective_has_growth_targets {
            if fast.growth_reward_count > 0 {
                running_score += i64::from(fast.growth_reward_count) * 25_000;
            }
            if full.missed_the_hunt_rewards > 0 {
                running_score -= i64::from(full.missed_the_hunt_rewards) * 30_000;
            }
            let all_growth_satisfied = root
                .growth_opportunity_targets
                .is_satisfied_by(&fast.growth_rewards);
            if !all_growth_satisfied && !lethal_incoming && full.alive_non_minion_enemies > 0 {
                if let Some(deck_budget) = full.active_growth_deck_budget {
                    if deck_budget > 0 {
                        let deck_bonus = (i64::from(deck_budget) * 6_000).min(24_000);
                        let hand_bonus = full
                            .active_growth_hand_budget
                            .filter(|&b| b > 0)
                            .map_or(0, |b| (i64::from(b) * 3_500).min(14_000));
                        if full.unblocked_incoming == 0 {
                            running_score += deck_bonus + hand_bonus;
                        } else {
                            let max_safe_dmg = deck_budget.min(2);
                            if full.unblocked_incoming <= max_safe_dmg
                                && 2 * i64::from(fast.player_hp - full.unblocked_incoming)
                                    >= i64::from(fast.player_max_hp)
                            {
                                let total_bonus = deck_bonus + hand_bonus;
                                running_score += if full.unblocked_incoming == 1 {
                                    total_bonus / 2
                                } else {
                                    total_bonus / 4
                                };
                            }
                        }
                    } else if full.unblocked_incoming == 0 && full.focus_target_remaining_hp <= 20
                    {
                        running_score += 4_000;
                        if full.active_growth_hand_budget.is_some() {
                            running_score += 4_000;
                        }
                    }
                }
            }
        } else if fast.growth_reward_count > 0 {
            running_score += i64::from(fast.growth_reward_count) * 5_000;
        }
    }

    if !root.active_relic_targets.is_empty() {
        running_score += compute_relic_guidance_score(
            fast.relic_eval.satisfied_priority,
            fast.relic_eval.distance,
        );
    }

    running_score -= i64::from((full.anger_copies_generated - 2).max(0)) * 2_000;
    running_score -= i64::from(action_count) * 5;

    if root.theft_policy == SolverTheftPolicy::PreserveResources {
        running_score -= i64::from(fast.outstanding_stolen) * 1_000_000;
        if fast.forfeits_gold {
            running_score -= 2_000_000;
        }
    }

    let mut score = running_score as f64;
    if let Some(p) = perturbation {
        if (p.scale - 1.0).abs() > f64::EPSILON {
            let term = match p.term {
                BeamWeightTerm::CurrentEnergy => fast.energy.clamp(0, 6) as f64 * 55_000.0,
                BeamWeightTerm::PersistentBuffDelta => eff_setup as f64 * setup_scale as f64,
                BeamWeightTerm::EnemyHp => -(fast.enemy_hp as f64) * enemy_hp_weight as f64,
            };
            score += (p.scale - 1.0) * term;
        }
    }

    score
}

#[derive(Clone, Copy, Debug)]
pub struct FinalPlanSelection {
    pub node_id: NodeId,
    pub turn: i32,
    pub won: bool,
    pub survives: bool,
    pub is_event_defeat: bool,
    pub death_save_use_count: i32,
    pub player_hp: i32,
    pub end_player_max_hp: i32,
    pub strategic_hp_deficit: i32,
    pub net_persistent_hp_delta_centi: i32,
    pub net_potion_opportunity_cost_centi: i32,
    pub hp_lost: i32,
    pub potion_count: usize,
    pub optional_potion_count: usize,
    pub optional_paid_potion_count: usize,
    pub optional_potion_strategic_cost: i32,
    pub optional_ambergris_count: i32,
    pub used_potion_slot_mask: u16,
    pub alchemize_count: u8,
    pub empty_potion_slots: u8,
    pub outstanding_stolen: i32,
    pub forfeits_gold: bool,
    pub score: i64,
    pub all_forced_uses_satisfied: bool,
    pub strategy_goal_hp_credit: i32,
    pub strategy_goal_count: i32,
    pub growth_rewards: GrowthCounters,
    pub relic_eval: RelicCounterEvaluation,
    pub recovered_hp: i32,
}

#[derive(Clone, Copy)]
pub(crate) struct EvaluatedCandidate {
    pub(crate) node_id: NodeId,
    pub(crate) quality: RouteQuality,
    pub(crate) explicit_potion_count: i32,
    pub(crate) forced_uses_satisfied: bool,
    pub(crate) forced_use_count: i32,
    pub(crate) optional_paid_potion_count: i32,
    pub(crate) optional_potion_strategic_cost: i32,
    pub(crate) optional_ambergris_count: i32,
    pub(crate) used_potion_slot_mask: u16,
    pub(crate) alchemize_count: u8,
    pub(crate) empty_potion_slots: u8,
    pub(crate) net_persistent_hp_delta_centi: i32,
    pub(crate) net_potion_opportunity_cost_centi: i32,
    pub(crate) end_player_max_hp: i32,
    pub(crate) raw_growth_reward_count: i32,
    pub(crate) rare: RareNodeEval,
    pub(crate) recovered_hp: i32,
    pub(crate) boundary_rank: u8,
    pub(crate) action_count: u16,
    pub(crate) turn: i32,
    pub(crate) player_hp: i32,
    pub(crate) state_hash: StateHash,
}

#[inline]
pub(crate) fn cmp_eval_cand(
    a: &EvaluatedCandidate,
    b: &EvaluatedCandidate,
    check_theft: bool,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let cmp = b
        .forced_uses_satisfied
        .cmp(&a.forced_uses_satisfied)
        .then_with(|| b.forced_use_count.cmp(&a.forced_use_count))
        .then_with(|| b.quality.won.cmp(&a.quality.won))
        .then_with(|| b.quality.survives.cmp(&a.quality.survives))
        .then_with(|| a.quality.is_event_defeat.cmp(&b.quality.is_event_defeat))
        .then_with(|| a.quality.death_save_use_count.cmp(&b.quality.death_save_use_count));
    if cmp != Ordering::Equal {
        return cmp;
    }
    if check_theft {
        let theft_cmp = a
            .quality
            .outstanding_stolen_resource
            .cmp(&b.quality.outstanding_stolen_resource)
            .then_with(|| {
                a.quality
                    .forfeits_encounter_gold
                    .cmp(&b.quality.forfeits_encounter_gold)
            });
        if theft_cmp != Ordering::Equal {
            return theft_cmp;
        }
    }

    // Defeat-only routes never burn paid potions merely to shave enemy HP;
    // compare lower potion cost/count first, then enemy HP, longer survival turn, and heuristic score.
    if !a.quality.won && !a.quality.survives && !b.quality.won && !b.quality.survives {
        return a
            .net_potion_opportunity_cost_centi
            .cmp(&b.net_potion_opportunity_cost_centi)
            .then_with(|| a.optional_paid_potion_count.cmp(&b.optional_paid_potion_count))
            .then_with(|| a.quality.enemy_hp.cmp(&b.quality.enemy_hp))
            .then_with(|| b.turn.cmp(&a.turn))
            .then_with(|| b.quality.score.total_cmp(&a.quality.score))
            .then_with(|| a.explicit_potion_count.cmp(&b.explicit_potion_count))
            .then_with(|| a.action_count.cmp(&b.action_count))
            .then_with(|| a.state_hash.cmp(&b.state_hash));
    }

    let a_v = (a.net_persistent_hp_delta_centi as i64)
        - (a.net_potion_opportunity_cost_centi as i64);
    let b_v = (b.net_persistent_hp_delta_centi as i64)
        - (b.net_potion_opportunity_cost_centi as i64);
    b_v.cmp(&a_v)
        .then_with(|| {
            b.net_persistent_hp_delta_centi
                .cmp(&a.net_persistent_hp_delta_centi)
        })
        .then_with(|| b.quality.growth_hp_credit.cmp(&a.quality.growth_hp_credit))
        .then_with(|| {
            b.quality
                .growth_reward_count
                .cmp(&a.quality.growth_reward_count)
        })
        .then_with(|| {
            a.quality
                .projected_battle_hp_lost
                .cmp(&b.quality.projected_battle_hp_lost)
        })
        .then_with(|| {
            a.quality
                .projected_battle_potion_count
                .cmp(&b.quality.projected_battle_potion_count)
        })
        .then_with(|| {
            a.quality
                .combat_ended_turn
                .unwrap_or(i32::MAX)
                .cmp(&b.quality.combat_ended_turn.unwrap_or(i32::MAX))
        })
        .then_with(|| b.raw_growth_reward_count.cmp(&a.raw_growth_reward_count))
        .then_with(|| a.boundary_rank.cmp(&b.boundary_rank))
        .then_with(|| a.explicit_potion_count.cmp(&b.explicit_potion_count))
        .then_with(|| a.quality.enemy_hp.cmp(&b.quality.enemy_hp))
        .then_with(|| b.quality.score.total_cmp(&a.quality.score))
        .then_with(|| a.action_count.cmp(&b.action_count))
        .then_with(|| a.state_hash.cmp(&b.state_hash))
}

pub(crate) struct FinalPlanTracker {
    effective_policy: SolverPotionPolicy,
    enforce_forced: bool,
    require_hard: bool,
    forced_directive_count: i32,
    check_theft: bool,
    theft_policy: Option<SolverTheftPolicy>,
    minimum_potion_uses: usize,
    pub(crate) best_hard: Option<EvaluatedCandidate>,
    pub(crate) best_any: Option<EvaluatedCandidate>,
}

impl FinalPlanTracker {
    pub(crate) fn new(
        root: &RootEvalBaseline,
        potion_policy: SolverPotionPolicy,
        theft_policy: Option<SolverTheftPolicy>,
        minimum_potion_uses: usize,
    ) -> Self {
        let effective_policy = match theft_policy {
            Some(SolverTheftPolicy::PreserveResources) => SolverPotionPolicy::Smart,
            _ => potion_policy,
        };
        let require_hard = root.all_forced_potion_mask != 0
            || (effective_policy == SolverPotionPolicy::RequireAtLeastOne
                && minimum_potion_uses > 0
                && root.has_eligible_usable_potion);
        Self {
            effective_policy,
            enforce_forced: root.all_forced_potion_mask != 0,
            require_hard,
            forced_directive_count: root.forced_directive_count as i32,
            check_theft: theft_policy == Some(SolverTheftPolicy::PreserveResources),
            theft_policy,
            minimum_potion_uses,
            best_hard: None,
            best_any: None,
        }
    }

    #[inline(always)]
    pub(crate) fn satisfies_hard_from_raw(
        &self,
        forced_uses_satisfied: bool,
        explicit_potion_count: i32,
        optional_paid_potion_count: i32,
    ) -> bool {
        let satisfies_policy_hard = match self.effective_policy {
            SolverPotionPolicy::Disabled => {
                optional_paid_potion_count == 0
                    && explicit_potion_count <= self.forced_directive_count
            }
            SolverPotionPolicy::RequireAtLeastOne => {
                explicit_potion_count >= self.minimum_potion_uses as i32
            }
            SolverPotionPolicy::Smart => true,
        };
        forced_uses_satisfied
            && explicit_potion_count >= self.minimum_potion_uses as i32
            && satisfies_policy_hard
    }

    #[inline(always)]
    pub(crate) fn can_fast_reject_non_victory(
        &self,
        f: &StoredFastEval,
        action_count: u16,
        forced_used_mask: u64,
        explicit_potion_count: u8,
        optional_paid_potion_count: u8,
        root: &RootEvalBaseline,
    ) -> bool {
        let Some(ref bh) = self.best_hard else {
            return false;
        };
        let complete_victory = !f.is_event_defeat()
            && RouteQualityPolicy::is_complete_victory(
                action_count as usize,
                f.all_enemies_dead() || f.won(),
                f.player_dead(),
                f.projected_player_hp as i32,
            );
        if bh.quality.won && !complete_victory {
            return true;
        }
        let forced_uses_satisfied =
            (forced_used_mask & root.all_forced_potion_mask) == root.all_forced_potion_mask;
        let satisfies_hard = self.satisfies_hard_from_raw(
            forced_uses_satisfied,
            explicit_potion_count as i32,
            optional_paid_potion_count as i32,
        );
        !satisfies_hard
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn evaluate_raw(
        &self,
        node_id: NodeId,
        f: &StoredFastEval,
        rare: RareNodeEval,
        action_count: u16,
        turn: i32,
        potion_count: u8,
        explicit_potion_count_u8: u8,
        optional_paid_potion_count_u8: u8,
        optional_paid_potion_cost: i16,
        ambergris_count: u8,
        forced_used_mask: u64,
        forced_ambergris_count_u8: u8,
        used_potion_slot_mask: u16,
        boundary_rank: u8,
        beam_rank_score: f64,
        state_hash: StateHash,
        root: &RootEvalBaseline,
    ) -> (EvaluatedCandidate, bool) {
        let explicit_potion_count = explicit_potion_count_u8 as i32;
        let matched_forced = forced_used_mask & root.all_forced_potion_mask;
        let forced_uses_satisfied = matched_forced == root.all_forced_potion_mask;
        let forced_use_count = if self.enforce_forced {
            matched_forced.count_ones() as i32
        } else {
            0
        };
        let forced_ambergris_count = if self.enforce_forced {
            forced_ambergris_count_u8 as i32
        } else {
            0
        };
        let optional_paid_potion_count = optional_paid_potion_count_u8 as i32;
        let optional_ambergris_count =
            (ambergris_count as i32 - forced_ambergris_count).max(0);
        let end_player_max_hp = if f.player_max_hp > 0 {
            f.player_max_hp as i32
        } else {
            root.initial_player_max_hp.max(1)
        };
        let complete_victory = !f.is_event_defeat()
            && RouteQualityPolicy::is_complete_victory(
                action_count as usize,
                f.all_enemies_dead() || f.won(),
                f.player_dead(),
                f.projected_player_hp as i32,
            );
        let relic_heal = ActEndingBossPolicy::ranked_post_combat_relic_heal(
            root.post_combat_relic_heal,
            complete_victory,
            f.player_hp as i32,
            end_player_max_hp,
        );
        let net_persistent_hp_delta_centi = ActEndingBossPolicy::compute_net_persistent_hp_delta_centi(
            f.player_hp as i32,
            relic_heal,
            root.initial_player_hp,
            end_player_max_hp,
            root.initial_player_max_hp,
            f.death_save_hp_restored as i32,
            f.strategic_hp_credit as i32,
            root.boss_hp_relief,
            root.ignore_long_term_rewards,
            i32::from(f.feed_count) * root.feed_max_hp_per_kill,
        );
        let net_potion_opportunity_cost_centi =
            PotionUsePolicy::compute_net_potion_opportunity_cost_centi(
                optional_paid_potion_cost as i32,
                optional_paid_potion_count,
                f.alchemize_count as i32,
                f.empty_potion_slots as i32,
                root,
            );
        let strategic_hp_deficit =
            ActEndingBossPolicy::round_centi_to_hp(-net_persistent_hp_delta_centi);
        let optional_potion_strategic_cost = if net_potion_opportunity_cost_centi > 0 {
            ActEndingBossPolicy::round_centi_to_hp(net_potion_opportunity_cost_centi).max(1)
        } else {
            0
        };
        let satisfies_hard = self.satisfies_hard_from_raw(
            forced_uses_satisfied,
            explicit_potion_count,
            optional_paid_potion_count,
        );
        let quality = RouteQuality {
            won: complete_victory,
            survives: !f.player_dead() && f.projected_player_hp > 0,
            is_event_defeat: f.is_event_defeat(),
            death_save_use_count: f.death_save_use_count as i32,
            outstanding_stolen_resource: f.outstanding_stolen as i32,
            strategic_hp_deficit,
            growth_hp_credit: f.strategy_goal_hp_credit as i32,
            growth_reward_count: f.strategy_goal_count as i32,
            combat_ended_turn: if complete_victory {
                f.combat_ended_turn()
            } else {
                None
            },
            projected_battle_hp_lost: f.cumulative_hp_lost as i32,
            potion_strategic_cost: optional_potion_strategic_cost,
            projected_battle_potion_count: potion_count as i32,
            enemy_hp: f.enemy_hp,
            score: beam_rank_score,
            theft_policy: self.theft_policy,
            forfeits_encounter_gold: f.forfeits_gold(),
        };
        let cand = EvaluatedCandidate {
            node_id,
            quality,
            explicit_potion_count,
            forced_uses_satisfied,
            forced_use_count,
            optional_paid_potion_count,
            optional_potion_strategic_cost,
            optional_ambergris_count,
            used_potion_slot_mask,
            alchemize_count: f.alchemize_count,
            empty_potion_slots: f.empty_potion_slots,
            net_persistent_hp_delta_centi,
            net_potion_opportunity_cost_centi,
            end_player_max_hp,
            raw_growth_reward_count: f.raw_growth_reward_count as i32,
            rare,
            recovered_hp: f.recovered_hp as i32,
            boundary_rank,
            action_count,
            turn,
            player_hp: f.player_hp as i32,
            state_hash,
        };
        (cand, satisfies_hard)
    }

    #[inline(always)]
    pub(crate) fn would_improve(&self, cand: &EvaluatedCandidate, satisfies_hard: bool) -> bool {
        if satisfies_hard {
            self.best_hard
                .as_ref()
                .is_none_or(|cur| cmp_eval_cand(cand, cur, self.check_theft).is_lt())
        } else {
            !self.require_hard
                && self.best_hard.is_none()
                && self
                    .best_any
                    .as_ref()
                    .is_none_or(|cur| cmp_eval_cand(cand, cur, self.check_theft).is_lt())
        }
    }

    #[inline]
    pub(crate) fn record(
        &mut self,
        nid: NodeId,
        mut cand: EvaluatedCandidate,
        satisfies_hard: bool,
    ) {
        cand.node_id = nid;
        if satisfies_hard {
            self.best_hard = Some(cand);
        } else if !self.require_hard && self.best_hard.is_none() {
            self.best_any = Some(cand);
        }
    }

    #[inline]
    pub(crate) fn observe_node(&mut self, arena: &NodeArena, nid: NodeId, root: &RootEvalBaseline) {
        let node = arena.get(nid);
        let rare = arena.rare_eval(nid);
        let (cand, satisfies_hard) = self.evaluate_raw(
            nid,
            &node.fast_eval,
            rare,
            node.action_count,
            node.turn as i32,
            node.potion_count,
            node.explicit_potion_count,
            node.optional_paid_potion_count,
            node.optional_paid_potion_cost,
            node.ambergris_count,
            node.forced_used_mask,
            node.forced_ambergris_count,
            node.used_potion_slot_mask,
            node.boundary_rank,
            node.beam_rank_score_or_base(),
            node.state_hash,
            root,
        );
        if self.would_improve(&cand, satisfies_hard) {
            self.record(nid, cand, satisfies_hard);
        }
    }
}

/// Unified centi-HP route selection matching `HotRouteQuality` + `HotPotionPolicy.SatisfiesHardConstraints`:
/// zero post-hoc Smart/Ambergris vetoes; ranks strictly by `V = NetPersistentHpDeltaCenti - NetPotionCostCenti`.
pub fn select_final_plan(
    arena: &NodeArena,
    candidates: &[NodeId],
    root: &RootEvalBaseline,
    potion_policy: SolverPotionPolicy,
    theft_policy: Option<SolverTheftPolicy>,
    _potion_replacement_hp_credit: i32,
    minimum_potion_uses: usize,
) -> Option<FinalPlanSelection> {
    if candidates.is_empty() {
        return None;
    }

    let mut tracker =
        FinalPlanTracker::new(root, potion_policy, theft_policy, minimum_potion_uses);
    for &nid in candidates {
        tracker.observe_node(arena, nid, root);
    }

    let best = if tracker.require_hard {
        tracker.best_hard?
    } else {
        tracker.best_hard.or(tracker.best_any)?
    };
    Some(FinalPlanSelection {
        node_id: best.node_id,
        turn: best.turn,
        won: best.quality.won,
        survives: best.quality.survives,
        is_event_defeat: best.quality.is_event_defeat,
        death_save_use_count: best.quality.death_save_use_count,
        player_hp: best.player_hp,
        end_player_max_hp: best.end_player_max_hp,
        strategic_hp_deficit: best.quality.strategic_hp_deficit,
        net_persistent_hp_delta_centi: best.net_persistent_hp_delta_centi,
        net_potion_opportunity_cost_centi: best.net_potion_opportunity_cost_centi,
        hp_lost: best.quality.projected_battle_hp_lost,
        potion_count: best.quality.projected_battle_potion_count as usize,
        optional_potion_count: best.optional_paid_potion_count.max(0) as usize,
        optional_paid_potion_count: best.optional_paid_potion_count.max(0) as usize,
        optional_potion_strategic_cost: best.optional_potion_strategic_cost,
        optional_ambergris_count: best.optional_ambergris_count,
        used_potion_slot_mask: best.used_potion_slot_mask,
        alchemize_count: best.alchemize_count,
        empty_potion_slots: best.empty_potion_slots,
        outstanding_stolen: best.quality.outstanding_stolen_resource,
        forfeits_gold: best.quality.forfeits_encounter_gold,
        score: best.quality.score.round() as i64,
        all_forced_uses_satisfied: best.forced_uses_satisfied,
        strategy_goal_hp_credit: best.quality.growth_hp_credit,
        strategy_goal_count: best.quality.growth_reward_count,
        growth_rewards: best.rare.growth_rewards,
        relic_eval: best.rare.relic_eval,
        recovered_hp: best.recovered_hp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arena::SearchNode;

    #[test]
    fn final_plan_tracker_matches_select_final_plan_across_mixed_terminals() {
        let mut root = RootEvalBaseline::default();
        root.initial_player_hp = 80;
        root.initial_player_max_hp = 80;
        root.all_forced_potion_mask = 0b01;
        root.forced_directive_count = 1;
        root.minimum_required_potion_uses = 1;

        let mut arena_all = NodeArena::default();
        let mut arena_filtered = NodeArena::default();
        let mut tracker = FinalPlanTracker::new(
            &root,
            SolverPotionPolicy::Smart,
            Some(SolverTheftPolicy::PreserveResources),
            1,
        );

        let make_node = |won: bool,
                         dead: bool,
                         hp: i16,
                         hp_lost: i16,
                         forced_mask: u64,
                         explicit_potions: u8,
                         opt_paid: u8,
                         turn: i16,
                         actions: u16,
                         hash: u64| {
            let mut f = StoredFastEval::default();
            f.player_hp = hp;
            f.player_max_hp = 80;
            f.projected_player_hp = hp;
            f.cumulative_hp_lost = hp_lost;
            if won {
                f.flags |= FAST_FLAG_WON | FAST_FLAG_ALL_ENEMIES_DEAD;
                f.combat_ended_turn_encoded = (turn as u16).saturating_add(1);
            }
            if dead {
                f.flags |= FAST_FLAG_PLAYER_DEAD;
            }
            SearchNode {
                state_hash: StateHash::from_parts(hash, hash.wrapping_mul(3)),
                forced_used_mask: forced_mask,
                beam_rank_score: 1000.0 - f64::from(hp_lost),
                fast_eval: f,
                turn,
                action_count: actions,
                potion_count: explicit_potions,
                explicit_potion_count: explicit_potions,
                optional_paid_potion_count: opt_paid,
                optional_paid_potion_cost: (opt_paid as i16) * 5,
                node_flags: crate::arena::NODE_FLAG_IS_TERMINAL,
                ..Default::default()
            }
        };

        // Sequence tests: non-hard, loss, worse win, better win, equal-tie, post-win loss, post-win non-hard
        let cases = [
            make_node(false, true, 0, 80, 0, 0, 0, 2, 4, 10),
            make_node(false, true, 0, 80, 1, 1, 0, 3, 5, 11),
            make_node(true, false, 60, 20, 1, 1, 1, 4, 9, 12),
            make_node(true, false, 70, 10, 1, 1, 0, 3, 7, 13),
            make_node(true, false, 70, 10, 1, 1, 0, 3, 7, 14), // loses tie-break on hash
            make_node(true, false, 70, 10, 1, 1, 0, 3, 7, 5),  // wins tie-break on hash
            make_node(false, true, 0, 80, 1, 1, 0, 4, 8, 1),
            make_node(true, false, 79, 1, 0, 0, 0, 2, 4, 2), // violates forced constraint
        ];

        let mut all_ids = Vec::new();
        let mut filtered_ids = Vec::new();
        for n in cases {
            all_ids.push(arena_all.push_node(n.clone()));
            let reject = !n.fast_eval.won()
                && tracker.can_fast_reject_non_victory(
                    &n.fast_eval,
                    n.action_count,
                    n.forced_used_mask,
                    n.explicit_potion_count,
                    n.optional_paid_potion_count,
                    &root,
                );
            if reject {
                continue;
            }
            let (cand, hard) = tracker.evaluate_raw(
                NodeId(0),
                &n.fast_eval,
                RareNodeEval::default(),
                n.action_count,
                n.turn as i32,
                n.potion_count,
                n.explicit_potion_count,
                n.optional_paid_potion_count,
                n.optional_paid_potion_cost,
                n.ambergris_count,
                n.forced_used_mask,
                n.forced_ambergris_count,
                n.used_potion_slot_mask,
                n.boundary_rank,
                n.beam_rank_score_or_base(),
                n.state_hash,
                &root,
            );
            if tracker.would_improve(&cand, hard) {
                let nid = arena_filtered.push_node(n);
                tracker.record(nid, cand, hard);
                filtered_ids.push(nid);
            }
        }

        assert!(filtered_ids.len() < all_ids.len());
        let sel_all = select_final_plan(
            &arena_all,
            &all_ids,
            &root,
            SolverPotionPolicy::Smart,
            Some(SolverTheftPolicy::PreserveResources),
            0,
            1,
        )
        .unwrap();
        let sel_filt = select_final_plan(
            &arena_filtered,
            &filtered_ids,
            &root,
            SolverPotionPolicy::Smart,
            Some(SolverTheftPolicy::PreserveResources),
            0,
            1,
        )
        .unwrap();
        assert_eq!(arena_all.get(sel_all.node_id).state_hash, arena_filtered.get(sel_filt.node_id).state_hash);
        assert_eq!(sel_all.won, sel_filt.won);
        assert_eq!(sel_all.player_hp, sel_filt.player_hp);
        assert_eq!(sel_all.hp_lost, sel_filt.hp_lost);
        assert_eq!(sel_all.net_persistent_hp_delta_centi, sel_filt.net_persistent_hp_delta_centi);
        assert_eq!(sel_all.net_potion_opportunity_cost_centi, sel_filt.net_potion_opportunity_cost_centi);
    }

    #[test]
    fn select_final_plan_returns_none_when_hard_potion_constraint_unsatisfied() {
        let mut root = RootEvalBaseline::default();
        root.initial_player_hp = 80;
        root.initial_player_max_hp = 80;
        root.all_forced_potion_mask = 0b01;
        root.forced_directive_count = 1;
        root.minimum_required_potion_uses = 1;

        let mut arena = NodeArena::default();
        let mut f = StoredFastEval::default();
        f.player_hp = 80;
        f.player_max_hp = 80;
        f.projected_player_hp = 80;
        f.cumulative_hp_lost = 0;
        f.flags |= FAST_FLAG_WON | FAST_FLAG_ALL_ENEMIES_DEAD;
        f.combat_ended_turn_encoded = 2;

        let non_compliant_win = arena.push_node(SearchNode {
            state_hash: StateHash::from_parts(1, 3),
            forced_used_mask: 0,
            beam_rank_score: 1000.0,
            fast_eval: f,
            turn: 1,
            action_count: 2,
            potion_count: 0,
            explicit_potion_count: 0,
            node_flags: crate::arena::NODE_FLAG_IS_TERMINAL,
            ..Default::default()
        });

        let selected = select_final_plan(
            &arena,
            &[non_compliant_win],
            &root,
            SolverPotionPolicy::Smart,
            None,
            0,
            1,
        );
        assert!(
            selected.is_none(),
            "must not fall back to best_any when a forced potion constraint is unsatisfied"
        );
    }

    #[test]
    fn evaluate_fast_and_full_fused_matches_fast_plus_full_exact() {
        use sts2_content::Sts2Content;
        use sts2_kernel::state::{
            ArrowSlotInst, BattleState, CardExtension, CardInst, CreatureInst, FieldMap,
            FieldValue, MonsterInst, OptCardHandle, OptI16, OrbInst, PlayerCounters, PlayerInst,
            PowerExtension, PowerInst, RelicInst, RootStaticContext,
        };
        use sts2_kernel::CardHandle;
        use sts2_types::{
            ArrowStatus, CharacterId, CombatSide, Dec, PlayerRngSet, PlayerTurnPhase, PotionId,
            Rng, RunRngSet,
        };

        let content = Sts2Content;
        let run_rngs = RunRngSet::from_string_seed("FUSED_EVAL_PARITY");
        let seed = run_rngs.seed;
        let pc_handle = CreatureHandle(1);
        let enemy_alive = CreatureHandle(2);
        let enemy_reviving = CreatureHandle(3);
        let enemy_escaped = CreatureHandle(4);

        let ctx = std::sync::Arc::new(RootStaticContext {
            encounter_id: "FUSED_TEST".to_owned(),
            room_type: RoomType::Monster,
            encounter_slots: vec!["s0".to_owned(), "s1".to_owned(), "s2".to_owned()],
            game_mode: GameMode::Standard,
            ascension: 10,
            character_id: CharacterId::Regent,
            ..Default::default()
        });

        let mut cards = Vec::new();
        let make_card = |idx: u32, id: CardId, pile: PileType, ext: Option<u32>, kw: CardKeywords| {
            let meta = content.card_meta(id).unwrap();
            let h = CardHandle(idx);
            CardInst {
                handle: h,
                id,
                upgrade_level: if id == CardId::Feed { 1 } else { 0 },
                canonical_base_energy_cost: meta.canonical_energy_cost as i16,
                base_energy_cost: meta.canonical_energy_cost as i16,
                captured_energy_x_value: 0,
                base_replay_count: 0,
                floor_added_to_deck: OptI16::some(1),
                deck_card_handle: OptCardHandle::some(h),
                clone_of_handle: OptCardHandle::NONE,
                temp_star_cost_override_this_turn: OptI16::NONE,
                temp_cost_override_this_turn: OptI16::NONE,
                temp_cost_override_this_turn_or_until_played: OptI16::NONE,
                temp_cost_override_this_combat: OptI16::NONE,
                local_keywords: meta.base_keywords | kw,
                keywords_added_from_outside: CardKeywords::empty(),
                keywords_removed_from_outside: CardKeywords::empty(),
                temp_flags: CardTempFlags::empty(),
                affliction: None,
                ext_index: ext,
                pile,
                removed_from_state: false,
                transform_into_after_play: None,
            }
        };

        let mut blade_fields = FieldMap::default();
        blade_fields.insert("SovereignBlade._damage", FieldValue::Int(22));
        let card_extensions = vec![CardExtension {
            local_energy_modifiers: Vec::new(),
            enchantment: None,
            private_fields: blade_fields,
        }];

        cards.push(make_card(1, CardId::StrikeIronclad, PileType::Hand, None, CardKeywords::empty()));
        cards.push(make_card(2, CardId::SovereignBlade, PileType::Hand, Some(0), CardKeywords::RETAIN));
        cards.push(make_card(3, CardId::Burn, PileType::Hand, None, CardKeywords::empty()));
        cards.push(make_card(4, CardId::DefendIronclad, PileType::Draw, None, CardKeywords::empty()));
        cards.push(make_card(5, CardId::Wound, PileType::Discard, None, CardKeywords::empty()));
        cards.push(make_card(6, CardId::Feed, PileType::Exhaust, None, CardKeywords::empty()));

        let mut thievery_fields = FieldMap::default();
        thievery_fields.insert("ThieveryPower._goldStolen", FieldValue::Int(18));
        let mut reviving_fields = FieldMap::default();
        reviving_fields.insert(hot_keys::REATTACH_IS_REVIVING, FieldValue::Bool(true));

        let power_extensions = vec![
            PowerExtension {
                private_fields: thievery_fields,
                attack_command_id: None,
                amount_when_attack_started: 0,
                impaled_arrows: SmallVec::new(),
                hit_counter: 0,
            },
            PowerExtension {
                private_fields: reviving_fields,
                attack_command_id: None,
                amount_when_attack_started: 0,
                impaled_arrows: SmallVec::new(),
                hit_counter: 0,
            },
        ];

        let mk_pow = |h: u32, id: PowerId, amt: i32, ext: Option<u32>| PowerInst {
            handle: h,
            id,
            amount: amt,
            amount_on_turn_start: amt,
            skip_next_duration_tick: false,
            applier_handle: None,
            ext_index: ext,
        };

        let mk_mon = || MonsterInst {
            monster_id: MonsterId::Nibbit,
            rng: Rng::from_seed(seed + 1),
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

        let pc = CreatureInst {
            handle: pc_handle,
            combat_id: Some(0),
            side: CombatSide::Player,
            slot_name: None,
            is_player: true,
            pet_owner_player_index: None,
            monster_id: None,
            current_hp: 62,
            max_hp: 80,
            block: 9,
            cumulative_hp_lost: 18,
            is_detached: false,
            monster: None,
            has_minion_power: false,
            powers: vec![
                mk_pow(1, PowerId::FocusPower, 3, None),
                mk_pow(2, PowerId::RegenPower, 4, None),
                mk_pow(3, PowerId::FurnacePower, 5, None),
                mk_pow(4, PowerId::PlatingPower, 6, None),
                mk_pow(5, PowerId::VulnerablePower, 2, None),
                mk_pow(6, PowerId::BlockNextTurnPower, 8, None),
                mk_pow(7, PowerId::BlurPower, 1, None),
            ],
        };
        let ec_alive = CreatureInst {
            handle: enemy_alive,
            combat_id: Some(1),
            side: CombatSide::Enemy,
            slot_name: None,
            is_player: false,
            pet_owner_player_index: None,
            monster_id: Some(MonsterId::Nibbit),
            current_hp: 75,
            max_hp: 85,
            block: 5,
            cumulative_hp_lost: 10,
            is_detached: false,
            monster: Some(Box::new(mk_mon())),
            has_minion_power: false,
            powers: vec![
                mk_pow(8, PowerId::PoisonPower, 7, None),
                mk_pow(9, PowerId::DoomPower, 15, None),
                mk_pow(10, PowerId::WeakPower, 2, None),
                mk_pow(11, PowerId::VulnerablePower, 1, None),
                mk_pow(12, PowerId::StrengthPower, 3, None),
                mk_pow(13, PowerId::SandpitPower, 3, None),
                mk_pow(14, PowerId::ThieveryPower, 0, Some(0)),
            ],
        };
        let ec_rev = CreatureInst {
            handle: enemy_reviving,
            combat_id: Some(2),
            side: CombatSide::Enemy,
            slot_name: None,
            is_player: false,
            pet_owner_player_index: None,
            monster_id: Some(MonsterId::Nibbit),
            current_hp: 0,
            max_hp: 44,
            block: 0,
            cumulative_hp_lost: 44,
            is_detached: false,
            monster: Some(Box::new(mk_mon())),
            has_minion_power: false,
            powers: vec![mk_pow(15, PowerId::ReattachPower, 22, Some(1))],
        };
        let ec_esc = CreatureInst {
            handle: enemy_escaped,
            combat_id: Some(3),
            side: CombatSide::Enemy,
            slot_name: None,
            is_player: false,
            pet_owner_player_index: None,
            monster_id: Some(MonsterId::Nibbit),
            current_hp: 30,
            max_hp: 30,
            block: 0,
            cumulative_hp_lost: 0,
            is_detached: true,
            monster: Some(Box::new(mk_mon())),
            has_minion_power: false,
            powers: vec![mk_pow(16, PowerId::SwipePower, 10, None)],
        };

        let player = PlayerInst {
            creature_handle: pc_handle,
            current_hp: 62,
            max_hp: 80,
            block: 9,
            cumulative_hp_lost: 18,
            gold: 100,
            max_energy: 3,
            base_orb_slot_count: 3,
            is_active_for_hooks: true,
            can_use_or_remove_potions: true,
            energy: 2,
            stars: 3,
            phase: PlayerTurnPhase::Play,
            turn_number: 2,
            counters: PlayerCounters {
                melody_cards_finished_this_turn: 1,
                ..Default::default()
            },
            hand: vec![CardHandle(1), CardHandle(2), CardHandle(3)],
            draw: vec![CardHandle(4)],
            discard: vec![CardHandle(5)],
            exhaust: vec![CardHandle(6)],
            play: Vec::new(),
            deck: vec![
                CardHandle(1),
                CardHandle(4),
                CardHandle(5),
                CardHandle(6),
            ],
            potion_slots: smallvec::smallvec![Some(PotionId::FirePotion), None, None],
            combat_generated_potion_mask: 0,
            orb_capacity: 3,
            orbs: vec![
                OrbInst {
                    id: OrbId::FrostOrb,
                    raw_evoke_value: None,
                    raw_passive_value: None,
                },
                OrbInst {
                    id: OrbId::DarkOrb,
                    raw_evoke_value: Some(Dec::from_i32(14)),
                    raw_passive_value: None,
                },
                OrbInst {
                    id: OrbId::LightningOrb,
                    raw_evoke_value: None,
                    raw_passive_value: None,
                },
            ],
            arrows: vec![
                ArrowSlotInst {
                    status: ArrowStatus::Retrieved,
                },
                ArrowSlotInst {
                    status: ArrowStatus::Away,
                },
            ],
            relics: vec![RelicInst {
                id: RelicId::GoldPlatedCables,
                is_wax: false,
                is_melted: false,
                stack_count: 1,
                floor_added_to_deck: None,
                private_fields: FieldMap::default(),
            }],
            card_or_potion_effect_depth: 0,
            death_save_use_count: 0,
            death_save_relic_hp_restored: 0,
            death_save_potion_hp_restored: 0,
        };

        let mut state = BattleState {
            static_ctx: ctx,
            current_side: CombatSide::Player,
            round_number: 2,
            is_in_progress: true,
            is_starting: false,
            won: false,
            is_player_extra_turn: false,
            gold_was_stolen: false,
            battleworn_dummy_timed_out: false,
            end_player_turn_requested: false,
            card_action_boundary_depth: 0,
            next_creature_id: 4,
            initial_encounter_monster_count: 3,
            next_card_handle: 7,
            next_creature_handle: 5,
            next_power_handle: 17,
            next_attack_id: 1,
            run_rngs,
            player_rngs: PlayerRngSet::from_seed(seed),
            player,
            cards,
            card_extensions,
            creatures: vec![pc, ec_alive, ec_rev, ec_esc],
            power_extensions,
            ally_handles: vec![pc_handle],
            enemy_handles: vec![enemy_alive, enemy_reviving],
            escaped_handles: smallvec::smallvec![enemy_escaped],
            removed_handles: SmallVec::new(),
            spawned_enemy_handles: smallvec::smallvec![enemy_alive, enemy_reviving],
            pet_handles: SmallVec::new(),
            attacking_creatures: SmallVec::new(),
            deferred_death_cleanup: SmallVec::new(),
            damage_history: Vec::new(),
            semantic_history: Default::default(),
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
        state.invalidate_hook_cache();
        state.ensure_hook_cache(&content).unwrap();
        state.semantic_history.growth_rewards.feed = 2;

        let baseline = RootEvalBaseline::from_state_with_policy(
            &state,
            &content,
            &crate::SearchPolicy::default(),
        );
        let mut intent_cache = IntentHitsCache::new();
        for &stand_pat in &[None, Some(48), Some(0)] {
            let (fast_a, full_a, rare_a) = evaluate_fast_and_full_fused(
                &state,
                &content,
                &baseline,
                3,
                1,
                stand_pat,
                None,
            );
            let (fast_b, full_b, rare_b) = evaluate_fast_and_full_fused(
                &state,
                &content,
                &baseline,
                3,
                1,
                stand_pat,
                Some(&mut intent_cache),
            );
            let fast_ref = evaluate_fast(&state, &content, &baseline, stand_pat);
            let full_ref = evaluate_full_details_cached(&state, &content, &fast_ref, &baseline, None);
            debug_assert_fused_parity(&fast_ref, &fast_a, &full_ref, &full_a);
            debug_assert_fused_parity(&fast_ref, &fast_b, &full_ref, &full_b);
            assert!(rare_a.is_some());
            assert_eq!(rare_a, rare_b);
            assert_eq!(fast_a.outstanding_stolen, 18);
            assert!(fast_a.forfeits_gold);
            assert!(full_a.retained_blade_bonus_base >= 27);
            assert!(full_a.orb_score > 0);
        }
    }

    #[test]
    fn stand_pat_cache_seqlock_concurrent_and_edge_cases() {
        let cache = StandPatCache::new();
        let zero_hash = StateHash(0);
        assert!(cache.probe(zero_hash, 0).is_none());

        let v0 = StandPatEvaluation {
            projected_hp: 55,
            death_save_hp_restored: 3,
            all_enemies_dead: false,
            resource_value: 24,
            delayed_damage: 9,
        };
        cache.insert(zero_hash, 0, v0);
        let hit0 = cache.probe(zero_hash, 0).expect("StateHash(0) round 0 hit");
        assert_eq!(hit0.projected_hp, 55);
        assert_eq!(hit0.death_save_hp_restored, 3);
        assert!(!hit0.all_enemies_dead);
        assert_eq!(hit0.resource_value, 24);
        assert_eq!(hit0.delayed_damage, 9);

        let same_set_hashes: Vec<StateHash> = (1u128..1_000_000u128)
            .filter_map(|k| {
                let cand = StateHash(k << 32);
                if StandPatCache::set_index(cand, 0) == StandPatCache::set_index(zero_hash, 0) {
                    Some(cand)
                } else {
                    None
                }
            })
            .take(5)
            .collect();
        assert_eq!(same_set_hashes.len(), 5);
        for (idx, &h) in same_set_hashes.iter().enumerate() {
            cache.insert(
                h,
                0,
                StandPatEvaluation {
                    projected_hp: 10 + idx as i32,
                    all_enemies_dead: idx % 2 == 0,
                    ..Default::default()
                },
            );
        }
        assert!(cache.probe(zero_hash, 0).is_none());
        assert!(cache.probe(same_set_hashes[4], 0).is_some());

        std::thread::scope(|s| {
            for t in 0..8usize {
                let c = &cache;
                s.spawn(move || {
                    for iter in 0..500u128 {
                        let key_id = (iter % 16) + 1;
                        let h = StateHash::from_parts(key_id as u64, (t as u64) & 1);
                        let rnd = (iter % 3) as i32;
                        let v = StandPatEvaluation {
                            projected_hp: (key_id as i32) * 10 + rnd,
                            death_save_hp_restored: rnd,
                            all_enemies_dead: (key_id & 1) == 0,
                            resource_value: (key_id as i32) * 3,
                            delayed_damage: (key_id as i32) + rnd,
                        };
                        c.insert(h, rnd, v);
                        if let Some(got) = c.probe(h, rnd) {
                            assert_eq!(got.projected_hp, v.projected_hp);
                            assert_eq!(got.death_save_hp_restored, v.death_save_hp_restored);
                            assert_eq!(got.all_enemies_dead, v.all_enemies_dead);
                            assert_eq!(got.resource_value, v.resource_value);
                            assert_eq!(got.delayed_damage, v.delayed_damage);
                        }
                    }
                });
            }
        });
    }

    fn build_eval_test_state(
        deck: &[CardId],
        relics: &[RelicId],
        potions: &[PotionId],
        enemies: &[(MonsterId, i32)],
        room_type: RoomType,
    ) -> BattleState {
        use sts2_content::Sts2Content;
        use sts2_kernel::state::{
            BattleState, CardInst, CreatureInst, MonsterInst, OptCardHandle, OptI16,
            PlayerCounters, PlayerInst, RootStaticContext,
        };
        use sts2_types::{CharacterId, CombatSide, PlayerRngSet, PlayerTurnPhase, Rng, RunRngSet};

        let content = Sts2Content;
        let run_rngs = RunRngSet::from_string_seed("EVAL_UNIT_SYNC_TESTS");
        let seed = run_rngs.seed;
        let pc_handle = CreatureHandle(1);

        let mut cards = Vec::with_capacity(deck.len());
        let mut deck_handles = Vec::with_capacity(deck.len());
        let mut hand = Vec::new();
        for (i, &cid) in deck.iter().enumerate() {
            let meta = content.card_meta(cid).unwrap();
            let h = CardHandle((i + 1) as u32);
            deck_handles.push(h);
            hand.push(h);
            cards.push(CardInst {
                handle: h,
                id: cid,
                upgrade_level: 0,
                canonical_base_energy_cost: meta.canonical_energy_cost as i16,
                base_energy_cost: meta.canonical_energy_cost as i16,
                captured_energy_x_value: 0,
                base_replay_count: 0,
                floor_added_to_deck: OptI16::some(1),
                deck_card_handle: OptCardHandle::some(h),
                clone_of_handle: OptCardHandle::NONE,
                temp_star_cost_override_this_turn: OptI16::NONE,
                temp_cost_override_this_turn: OptI16::NONE,
                temp_cost_override_this_turn_or_until_played: OptI16::NONE,
                temp_cost_override_this_combat: OptI16::NONE,
                local_keywords: meta.base_keywords,
                keywords_added_from_outside: CardKeywords::empty(),
                keywords_removed_from_outside: CardKeywords::empty(),
                temp_flags: CardTempFlags::empty(),
                affliction: None,
                ext_index: None,
                pile: PileType::Hand,
                removed_from_state: false,
                transform_into_after_play: None,
            });
        }

        let mut creatures = vec![CreatureInst {
            handle: pc_handle,
            combat_id: Some(0),
            side: CombatSide::Player,
            slot_name: None,
            is_player: true,
            pet_owner_player_index: None,
            monster_id: None,
            current_hp: 70,
            max_hp: 80,
            block: 0,
            cumulative_hp_lost: 10,
            is_detached: false,
            monster: None,
            has_minion_power: false,
            powers: Vec::new(),
        }];

        let mut enemy_handles = Vec::new();
        let mut encounter_slots = Vec::new();
        for (idx, &(mid, hp)) in enemies.iter().enumerate() {
            let eh = CreatureHandle((idx + 2) as u32);
            enemy_handles.push(eh);
            encounter_slots.push(format!("slot_{idx}"));
            creatures.push(CreatureInst {
                handle: eh,
                combat_id: Some((idx + 1) as u32),
                side: CombatSide::Enemy,
                slot_name: None,
                is_player: false,
                pet_owner_player_index: None,
                monster_id: Some(mid),
                current_hp: hp,
                max_hp: hp,
                block: 0,
                cumulative_hp_lost: 0,
                is_detached: false,
                monster: Some(Box::new(MonsterInst {
                    monster_id: mid,
                    rng: Rng::from_seed(seed + 1 + idx as u64),
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
                })),
                has_minion_power: false,
                powers: Vec::new(),
            });
        }

        let mut potion_slots: smallvec::SmallVec<[Option<PotionId>; 5]> =
            smallvec::smallvec![None, None, None];
        for (i, &pid) in potions.iter().take(3).enumerate() {
            potion_slots[i] = Some(pid);
        }

        let player = PlayerInst {
            creature_handle: pc_handle,
            current_hp: 70,
            max_hp: 80,
            block: 0,
            cumulative_hp_lost: 10,
            gold: 100,
            max_energy: 3,
            base_orb_slot_count: 0,
            is_active_for_hooks: true,
            can_use_or_remove_potions: true,
            energy: 3,
            stars: 0,
            phase: PlayerTurnPhase::Play,
            turn_number: 1,
            counters: PlayerCounters::default(),
            hand,
            draw: Vec::new(),
            discard: Vec::new(),
            exhaust: Vec::new(),
            play: Vec::new(),
            deck: deck_handles,
            potion_slots,
            combat_generated_potion_mask: 0,
            orb_capacity: 0,
            orbs: Vec::new(),
            arrows: Vec::new(),
            relics: relics
                .iter()
                .map(|&id| sts2_kernel::state::RelicInst {
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

        let mut state = BattleState {
            static_ctx: std::sync::Arc::new(RootStaticContext {
                encounter_id: "EVAL_UNIT_SYNC".to_owned(),
                room_type,
                encounter_slots,
                game_mode: GameMode::Standard,
                ascension: 0,
                character_id: CharacterId::Ironclad,
                ..Default::default()
            }),
            current_side: CombatSide::Player,
            round_number: 1,
            is_in_progress: true,
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
            next_power_handle: 100,
            next_attack_id: 1,
            run_rngs,
            player_rngs: PlayerRngSet::from_seed(seed),
            player,
            cards,
            card_extensions: Vec::new(),
            creatures,
            power_extensions: Vec::new(),
            ally_handles: vec![pc_handle],
            enemy_handles: enemy_handles.clone(),
            escaped_handles: smallvec::SmallVec::new(),
            removed_handles: smallvec::SmallVec::new(),
            spawned_enemy_handles: smallvec::SmallVec::from_vec(enemy_handles),
            pet_handles: smallvec::SmallVec::new(),
            attacking_creatures: smallvec::SmallVec::new(),
            deferred_death_cleanup: smallvec::SmallVec::new(),
            damage_history: Vec::new(),
            semantic_history: Default::default(),
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
        state.invalidate_hook_cache();
        state.ensure_hook_cache(&content).unwrap();
        state
    }

    #[test]
    fn death_save_wiring_and_mode_penalties_match_hot_evaluator() {
        use sts2_content::Sts2Content;
        let content = Sts2Content;
        let mut state = build_eval_test_state(
            &[CardId::StrikeIronclad],
            &[],
            &[],
            &[(MonsterId::Nibbit, 30)],
            RoomType::Monster,
        );
        state.player.death_save_use_count = 1;
        state.player.death_save_relic_hp_restored = 24;
        state.player.death_save_potion_hp_restored = 6;
        let root = RootEvalBaseline::from_state_with_policy(
            &state,
            &content,
            &crate::SearchPolicy::default(),
        );
        let (fast, full, _) =
            evaluate_fast_and_full_fused(&state, &content, &root, 0, 0, None, None);
        assert_eq!(fast.death_save_use_count, 1);
        assert_eq!(fast.death_save_hp_restored, 30);

        let mut scratch = state.clone();
        let mut pack_buf = Vec::new();
        let sp = StandPatCache::new().get_or_compute(
            StateHash(777),
            1,
            &{
                let mut term = state.clone();
                term.is_in_progress = false;
                term.won = true;
                term
            },
            &content,
            &mut scratch,
            &mut pack_buf,
        );
        assert_eq!(sp.death_save_hp_restored, 30);

        let s_bal = compute_beam_rank_score_with_centi(
            &fast,
            &full,
            &root,
            false,
            false,
            None,
            EvalMode::Balanced,
            0,
            0,
        );
        let s_surv = compute_beam_rank_score_with_centi(
            &fast,
            &full,
            &root,
            false,
            false,
            None,
            EvalMode::SetupAndSurvival,
            0,
            0,
        );
        let s_off = compute_beam_rank_score_with_centi(
            &fast,
            &full,
            &root,
            false,
            false,
            None,
            EvalMode::Offense,
            0,
            0,
        );
        let s_rescue = compute_beam_rank_score_with_centi(
            &fast,
            &full,
            &root,
            false,
            false,
            None,
            EvalMode::DesperateRescue,
            0,
            0,
        );
        // Balanced / SetupAndSurvival apply 500_000_000 death-save penalty; Offense / DesperateRescue apply 1_500_000.
        assert!(s_bal < -400_000_000.0);
        assert!(s_surv < -400_000_000.0);
        assert!(s_off > -100_000_000.0);
        assert!(s_rescue > -100_000_000.0);
    }

    #[test]
    fn cmp_eval_cand_prefers_lower_hp_loss_before_turn_and_preserves_defeat_potions() {
        let mk = |won: bool,
                  survives: bool,
                  net_hp: i32,
                  pot_centi: i32,
                  hp_lost: i32,
                  end_turn: Option<i32>,
                  turn: i32,
                  enemy_hp: i32,
                  opt_paid: i32,
                  hash: u64| EvaluatedCandidate {
            node_id: NodeId(hash as u32),
            quality: RouteQuality {
                won,
                survives,
                projected_battle_hp_lost: hp_lost,
                combat_ended_turn: end_turn,
                enemy_hp,
                score: 100.0,
                ..Default::default()
            },
            explicit_potion_count: opt_paid,
            forced_uses_satisfied: true,
            forced_use_count: 0,
            optional_paid_potion_count: opt_paid,
            optional_potion_strategic_cost: pot_centi / 100,
            optional_ambergris_count: 0,
            used_potion_slot_mask: 0,
            alchemize_count: 0,
            empty_potion_slots: 0,
            net_persistent_hp_delta_centi: net_hp,
            net_potion_opportunity_cost_centi: pot_centi,
            end_player_max_hp: 80,
            raw_growth_reward_count: 0,
            rare: RareNodeEval::default(),
            recovered_hp: 0,
            boundary_rank: 0,
            action_count: 3,
            turn,
            player_hp: if survives { 70 } else { 0 },
            state_hash: StateHash::from_parts(hash, 1),
        };

        // 1. Equal V & NetPersistentHpDelta: lower projected_battle_hp_lost wins over faster combat_ended_turn.
        let lower_hp_slower = mk(true, true, 0, 0, 2, Some(4), 4, 0, 0, 1);
        let higher_hp_faster = mk(true, true, 0, 0, 8, Some(1), 1, 0, 0, 2);
        assert!(cmp_eval_cand(&lower_hp_slower, &higher_hp_faster, false).is_lt());

        // 2. Defeat-only: lower potion cost beats higher potion cost even if costlier potion dealt more damage / boosted V via growth.
        let defeat_no_pot = mk(false, false, -7000, 0, 70, None, 2, 40, 0, 3);
        let defeat_paid_pot = mk(false, false, -5000, 900, 70, None, 4, 10, 1, 4);
        assert!(cmp_eval_cand(&defeat_no_pot, &defeat_paid_pot, false).is_lt());

        // 3. Defeat-only equal potion cost: lower enemy HP then longer survival turn wins.
        let defeat_more_dmg = mk(false, false, -7000, 0, 70, None, 2, 15, 0, 5);
        let defeat_less_dmg = mk(false, false, -7000, 0, 70, None, 5, 30, 0, 6);
        assert!(cmp_eval_cand(&defeat_more_dmg, &defeat_less_dmg, false).is_lt());
    }

    #[test]
    fn ongoing_boss_hp_relief_and_lethal_playable_cards_guard() {
        use sts2_content::Sts2Content;
        use sts2_kernel::state::PowerInst;

        let content = Sts2Content;
        // Player has 2 energy but ONLY a 3-cost Bash-equivalent or Unplayable Burn in hand -> playable_hand_count == 0.
        let mut state = build_eval_test_state(
            &[CardId::Burn, CardId::MeteorStrike],
            &[],
            &[],
            &[(MonsterId::Nibbit, 80)],
            RoomType::Boss,
        );
        state.player.energy = 2;
        state.player.current_hp = 10;
        state.creatures[0].current_hp = 10;
        // DisintegrationPower(12) guarantees lethal unblocked incoming (12 >= 10) while Burn deals 2 (total 14).
        state.creatures[0].powers.push(PowerInst {
            handle: 1,
            id: PowerId::DisintegrationPower,
            amount: 12,
            amount_on_turn_start: 12,
            skip_next_duration_tick: false,
            applier_handle: None,
            ext_index: None,
        });

        let policy_boss = crate::SearchPolicy {
            act_transition_boss_hp_strategy: BossHpStrategy::ProgressionFirst,
            final_boss_hp_strategy: BossHpStrategy::ProgressionFirst,
            is_non_final_act3_boss: false,
            ..Default::default()
        };
        let root_boss = RootEvalBaseline::from_state_with_policy(&state, &content, &policy_boss);
        assert_eq!(root_boss.boss_hp_relief, BossHpRelief::ActClearHeal);
        assert!(root_boss.is_act_ending_boss);

        // Non-final Act 3 boss must set is_act_ending_boss = false.
        let mut state_act3 = state.clone();
        std::sync::Arc::make_mut(&mut state_act3.static_ctx).act_index = 2;
        let root_act3_nonfinal = RootEvalBaseline::from_state_with_policy(
            &state_act3,
            &content,
            &crate::SearchPolicy {
                is_non_final_act3_boss: true,
                ..Default::default()
            },
        );
        assert!(!root_act3_nonfinal.is_act_ending_boss);
        assert_eq!(root_act3_nonfinal.encounter_boss_hp_relief, BossHpRelief::None);

        let (fast0, full0, _) =
            evaluate_fast_and_full_fused(&state, &content, &root_boss, 0, 0, None, None);
        assert_eq!(fast0.playable_hand_count, 0);
        assert_eq!(full0.playable_card_count, 0);
        assert!(full0.unblocked_incoming >= 14);
        assert!(full0.tactical_sig != 0);

        let mut state_playable = state.clone();
        state_playable.player.energy = 5; // MeteorStrike becomes playable!
        let (fast1, full1, _) =
            evaluate_fast_and_full_fused(&state_playable, &content, &root_boss, 0, 0, None, None);
        assert_eq!(fast1.playable_hand_count, 1);

        let s0 = compute_beam_rank_score_with_centi(
            &fast0,
            &full0,
            &root_boss,
            false,
            false,
            None,
            EvalMode::Balanced,
            0,
            0,
        );
        let s1 = compute_beam_rank_score_with_centi(
            &fast1,
            &full1,
            &root_boss,
            false,
            false,
            None,
            EvalMode::Balanced,
            0,
            0,
        );
        // Crossing from playable_cards == 0 to playable_cards == 1 removes the 3_200_000 lethal guard jump.
        assert!(s1 - s0 > 3_000_000.0);

        // Balanced must NOT apply the 1/5 BossHpRelief threat discount on non-lethal damage.
        let mut state_nonlethal = build_eval_test_state(
            &[CardId::StrikeIronclad],
            &[],
            &[],
            &[(MonsterId::Nibbit, 80)],
            RoomType::Boss,
        );
        state_nonlethal.creatures[0].powers.push(PowerInst {
            handle: 2,
            id: PowerId::DisintegrationPower,
            amount: 5,
            amount_on_turn_start: 5,
            skip_next_duration_tick: false,
            applier_handle: None,
            ext_index: None,
        });
        let root_nl_boss = RootEvalBaseline::from_state_with_policy(&state_nonlethal, &content, &policy_boss);
        let root_no_relief = RootEvalBaseline::from_state_with_policy(
            &state_nonlethal,
            &content,
            &crate::SearchPolicy {
                act_transition_boss_hp_strategy: BossHpStrategy::MinimizeHpLoss,
                ..Default::default()
            },
        );
        let (fnl, full_nl, _) = evaluate_fast_and_full_fused(
            &state_nonlethal,
            &content,
            &root_nl_boss,
            0,
            0,
            None,
            None,
        );
        let bal_boss = compute_beam_rank_score_with_centi(
            &fnl,
            &full_nl,
            &root_nl_boss,
            false,
            false,
            None,
            EvalMode::Balanced,
            0,
            0,
        );
        let bal_std = compute_beam_rank_score_with_centi(
            &fnl,
            &full_nl,
            &root_no_relief,
            false,
            false,
            None,
            EvalMode::Balanced,
            0,
            0,
        );
        assert!((bal_boss - bal_std).abs() < 1e-6);

        let surv_boss = compute_beam_rank_score_with_centi(
            &fnl,
            &full_nl,
            &root_nl_boss,
            false,
            false,
            None,
            EvalMode::SetupAndSurvival,
            0,
            0,
        );
        let surv_std = compute_beam_rank_score_with_centi(
            &fnl,
            &full_nl,
            &root_no_relief,
            false,
            false,
            None,
            EvalMode::SetupAndSurvival,
            0,
            0,
        );
        assert!((surv_boss - surv_std).abs() < 1e-6);

        let off_boss = compute_beam_rank_score_with_centi(
            &fnl,
            &full_nl,
            &root_nl_boss,
            false,
            false,
            None,
            EvalMode::Offense,
            0,
            0,
        );
        let off_std = compute_beam_rank_score_with_centi(
            &fnl,
            &full_nl,
            &root_no_relief,
            false,
            false,
            None,
            EvalMode::Offense,
            0,
            0,
        );
        // Offense discounts ongoing non-lethal threat to 55% under BossHpRelief::ActClearHeal.
        assert!(off_boss > off_std);

        let resc_boss = compute_beam_rank_score_with_centi(
            &fnl,
            &full_nl,
            &root_nl_boss,
            false,
            false,
            None,
            EvalMode::DesperateRescue,
            0,
            0,
        );
        let resc_std = compute_beam_rank_score_with_centi(
            &fnl,
            &full_nl,
            &root_no_relief,
            false,
            false,
            None,
            EvalMode::DesperateRescue,
            0,
            0,
        );
        // DesperateRescue discounts ongoing non-lethal threat to 40% under BossHpRelief::ActClearHeal.
        assert!(resc_boss > resc_std);
    }

    #[test]
    fn hit_by_hit_buffer_temp_str_poison_cap_and_potion_improve_bounds() {
        use sts2_content::Sts2Content;
        use sts2_kernel::state::PowerInst;

        let content = Sts2Content;
        // 1. Buffer absorbs 1 self-hazard + multi-hit vs single-hit + temporary ShacklingPotion vs permanent -Strength.
        let mut state = build_eval_test_state(
            &[CardId::Burn, CardId::TheHunt],
            &[RelicId::ArtOfWar, RelicId::Pocketwatch, RelicId::PaelsLegion],
            &[PotionId::BloodPotion, PotionId::FirePotion],
            &[(MonsterId::Queen, 50)],
            RoomType::Monster,
        );
        // Queen intending ENRAGE_MOVE with amalgam died -> 0 current incoming, activated phase 5x3 (15) +2 str = 25!
        let m = state.creatures[1].monster.as_deref_mut().unwrap();
        m.current_state_id = Cow::Borrowed("ENRAGE_MOVE");
        m.next_move_id = Some(Cow::Borrowed("ENRAGE_MOVE"));
        m.private_fields
            .insert("Queen._hasAmalgamDied", FieldValue::Bool(true));

        state.creatures[0].powers.push(PowerInst {
            handle: 10,
            id: PowerId::BufferPower,
            amount: 1,
            amount_on_turn_start: 1,
            skip_next_duration_tick: false,
            applier_handle: None,
            ext_index: None,
        });
        state.creatures[1].powers.push(PowerInst {
            handle: 11,
            id: PowerId::StrengthPower,
            amount: -4,
            amount_on_turn_start: -4,
            skip_next_duration_tick: false,
            applier_handle: None,
            ext_index: None,
        });
        state.creatures[1].powers.push(PowerInst {
            handle: 12,
            id: PowerId::ShacklingPotionPower,
            amount: 4,
            amount_on_turn_start: 4,
            skip_next_duration_tick: false,
            applier_handle: None,
            ext_index: None,
        });

        let root = RootEvalBaseline::from_state_with_policy(
            &state,
            &content,
            &crate::SearchPolicy::default(),
        );
        let (fast, full, _) =
            evaluate_fast_and_full_fused(&state, &content, &root, 0, 0, None, None);
        // Burn(2) consumed Buffer(1) => unblocked_incoming == 0!
        assert_eq!(full.unblocked_incoming, 0);
        // ShacklingPotion recovers the -4 Strength -> permanent strength suppression is 0!
        assert_eq!(full.enemy_strength_suppression, 0);
        // Queen phase threat detected while raw incoming == 0!
        assert!(full.enrage_phase_threat_dmg > 0);
        // ArtOfWar (+1 energy), Pocketwatch (+3 draw), PaelsLegion ready!
        assert_eq!(full.next_turn_energy, 1);
        assert_eq!(full.next_turn_draw, 3);
        assert!(full.paels_legion_ready);
        assert!(!fast.all_strategy_targets_satisfied);
        assert_ne!(full.tactical_sig, 0);

        // 2. Effective defense is capped when Barricade/Blur/Scaler absent.
        let mut s_def = state.clone();
        s_def.creatures[0].block = 100;
        let (fd, fulld, _) =
            evaluate_fast_and_full_fused(&s_def, &content, &root, 0, 0, None, None);
        assert!(fd.player_block == 100);
        assert_eq!(fulld.effective_defense, fulld.incoming_damage);

        // 3. Alchemize opportunity cost avoids double counting when growth budget covers standard potion value.
        let mut root_alc = root.clone();
        root_alc.belt_full_at_root = false;
        root_alc.empty_root_potion_slots = 0;
        root_alc.effective_growth_budgets.alchemize = 9;
        assert_eq!(
            PotionUsePolicy::compute_net_potion_opportunity_cost_centi(9, 1, 1, 1, &root_alc),
            900
        );
        root_alc.effective_growth_budgets.alchemize = 4;
        assert_eq!(
            PotionUsePolicy::compute_net_potion_opportunity_cost_centi(9, 1, 1, 1, &root_alc),
            400
        );

        // 4. Bounded BloodPotion heal + unrealized TheHunt growth allowance in can_any_paid_potion_improve_on.
        let mut root_hunt = build_eval_test_state(
            &[CardId::StrikeIronclad],
            &[],
            &[PotionId::FirePotion, PotionId::Duplicator],
            &[(MonsterId::Nibbit, 20)],
            RoomType::Monster,
        );
        root_hunt.player.current_hp = 80;
        root_hunt.creatures[0].current_hp = 80;
        let policy_bounded_growth = crate::SearchPolicy {
            potion_policy: SolverPotionPolicy::Smart,
            growth_budgets: GrowthCounters {
                hand_of_greed: 6,
                ..Default::default()
            },
            ..Default::default()
        };
        // With FirePotion cost=9 vs Duplicator cost=18, using FirePotion (raw cost=9) still leaves Duplicator (18 > 9),
        // and lossless win with all targets satisfied returns false!
        let mut st_clean = build_eval_test_state(
            &[CardId::HandOfGreed],
            &[],
            &[PotionId::FirePotion],
            &[(MonsterId::Nibbit, 20)],
            RoomType::Monster,
        );
        st_clean.player.current_hp = 80;
        st_clean.creatures[0].current_hp = 80;
        let rb_clean =
            RootEvalBaseline::from_state_with_policy(&st_clean, &content, &policy_bounded_growth);
        // 0-growth baseline (credit 0 < max 6) -> unrealized 6 < cost 9 -> cannot pay 9 HP potion just for 6 HP growth!
        assert!(!PotionUsePolicy::can_any_paid_potion_improve_on(
            PaidPotionImproveInput {
                won: true,
                survives: true,
                end_player_hp: 80,
                end_player_max_hp: 80,
                all_strategy_targets_satisfied: false,
                all_growth_targets_satisfied: false,
                growth_hp_credit: 0,
                ..Default::default()
            },
            &rb_clean,
        ));

        // 5. Swap check when costlier paid slot was spent while a cheaper paid slot remains,
        //    plus bounded BloodPotion + MeatOnTheBone cap.
        assert!(PotionUsePolicy::can_any_paid_potion_improve_on(
            PaidPotionImproveInput {
                won: true,
                survives: true,
                end_player_hp: 80,
                end_player_max_hp: 80,
                used_potion_slot_mask: 0b10,
                optional_paid_potion_count: 1,
                raw_potion_strategic_cost: 18,
                net_potion_opportunity_cost_centi: 1800,
                ..Default::default()
            },
            &RootEvalBaseline::from_state_with_policy(
                &root_hunt,
                &content,
                &crate::SearchPolicy {
                    potion_policy: SolverPotionPolicy::Smart,
                    ..Default::default()
                },
            ),
        ));

        let mut blood_state = build_eval_test_state(
            &[CardId::StrikeIronclad],
            &[RelicId::MeatOnTheBone],
            &[PotionId::BloodPotion],
            &[(MonsterId::Nibbit, 10)],
            RoomType::Monster,
        );
        blood_state.player.current_hp = 70;
        blood_state.player.max_hp = 80;
        blood_state.creatures[0].current_hp = 70;
        blood_state.creatures[0].max_hp = 80;
        let rb_blood = RootEvalBaseline::from_state_with_policy(
            &blood_state,
            &content,
            &crate::SearchPolicy {
                potion_policy: SolverPotionPolicy::Smart,
                ..Default::default()
            },
        );
        // BloodPotion heals up to 16 HP (from 70 -> 80 = at most 10 HP; end at 70 => max headroom 10 < BloodPotion cost 17).
        assert!(!PotionUsePolicy::can_any_paid_potion_improve_on(
            PaidPotionImproveInput {
                won: true,
                survives: true,
                end_player_hp: 70,
                end_player_max_hp: 80,
                ..Default::default()
            },
            &rb_blood,
        ));
    }

    #[test]
    fn nightmare_corruption_paels_eye_adaptable_poison_and_discard_invariant_sig() {
        use sts2_content::Sts2Content;
        use sts2_kernel::state::{FieldMap, PowerExtension, PowerInst};

        let content = Sts2Content;
        let mut s = build_eval_test_state(
            &[
                CardId::DefendIronclad,
                CardId::ShrugItOff,
                CardId::StrikeIronclad,
                CardId::DemonForm,
            ],
            &[RelicId::PaelsEye, RelicId::RunicPyramid],
            &[],
            &[(MonsterId::Nibbit, 20)],
            RoomType::Monster,
        );
        s.player.energy = 0; // ShrugItOff (1 cost skill) becomes playable only with Corruption!
        let demon_h = s.player.hand.pop().unwrap();
        s.cards[3].pile = PileType::Draw;
        s.player.draw.push(demon_h);
        let strike_h = s.player.hand.pop().unwrap();
        s.cards[2].pile = PileType::Exhaust;
        s.player.exhaust.push(strike_h);
        // Add a non-Basic Attack (PommelStrike, no Exhaust keyword) in exhaust to verify PaelsEye._usedThisCombat exemption.
        let non_basic_ex_h = CardHandle(50);
        let pommel_meta = content.card_meta(CardId::PommelStrike).unwrap();
        s.cards.push(sts2_kernel::state::CardInst {
            handle: non_basic_ex_h,
            id: CardId::PommelStrike,
            upgrade_level: 0,
            canonical_base_energy_cost: pommel_meta.canonical_energy_cost as i16,
            base_energy_cost: pommel_meta.canonical_energy_cost as i16,
            captured_energy_x_value: 0,
            base_replay_count: 0,
            floor_added_to_deck: sts2_kernel::OptI16::some(1),
            deck_card_handle: sts2_kernel::OptCardHandle::some(non_basic_ex_h),
            clone_of_handle: sts2_kernel::OptCardHandle::NONE,
            temp_star_cost_override_this_turn: sts2_kernel::OptI16::NONE,
            temp_cost_override_this_turn: sts2_kernel::OptI16::NONE,
            temp_cost_override_this_turn_or_until_played: sts2_kernel::OptI16::NONE,
            temp_cost_override_this_combat: sts2_kernel::OptI16::NONE,
            local_keywords: pommel_meta.base_keywords,
            keywords_added_from_outside: CardKeywords::empty(),
            keywords_removed_from_outside: CardKeywords::empty(),
            temp_flags: CardTempFlags::empty(),
            affliction: None,
            ext_index: None,
            pile: PileType::Exhaust,
            removed_from_state: false,
            transform_into_after_play: None,
        });
        s.player.exhaust.push(non_basic_ex_h);
        s.player.deck.push(non_basic_ex_h);
        s.player.relics[0]
            .private_fields
            .insert("PaelsEye._usedThisCombat", FieldValue::Bool(true));

        let mut ext_fields = FieldMap::default();
        ext_fields.insert("NightmarePower._selectedCard", FieldValue::CardHandle(demon_h.0 as i32));
        s.power_extensions.push(PowerExtension {
            private_fields: ext_fields,
            attack_command_id: None,
            amount_when_attack_started: 0,
            impaled_arrows: smallvec::SmallVec::new(),
            hit_counter: 0,
        });
        s.creatures[0].powers.push(PowerInst {
            handle: 200,
            id: PowerId::CorruptionPower,
            amount: 1,
            amount_on_turn_start: 1,
            skip_next_duration_tick: false,
            applier_handle: None,
            ext_index: None,
        });
        s.creatures[0].powers.push(PowerInst {
            handle: 201,
            id: PowerId::NightmarePower,
            amount: 3,
            amount_on_turn_start: 3,
            skip_next_duration_tick: false,
            applier_handle: None,
            ext_index: Some(0),
        });
        s.creatures[0].powers.push(PowerInst {
            handle: 202,
            id: PowerId::WasteAwayPower,
            amount: 2,
            amount_on_turn_start: 2,
            skip_next_duration_tick: false,
            applier_handle: None,
            ext_index: None,
        });
        // Poison(25) >= HP(20): normally poison_lethal, unless AdaptablePower is active!
        s.creatures[1].powers.push(PowerInst {
            handle: 203,
            id: PowerId::PoisonPower,
            amount: 25,
            amount_on_turn_start: 25,
            skip_next_duration_tick: false,
            applier_handle: None,
            ext_index: None,
        });

        let root = RootEvalBaseline::from_state_with_policy(
            &s,
            &content,
            &crate::SearchPolicy::default(),
        );
        let (f_no_adapt, full_no_adapt, _) =
            evaluate_fast_and_full_fused(&s, &content, &root, 0, 0, None, None);
        assert_eq!(full_no_adapt.poison_lethal_count, 1);
        // Corruption makes ShrugItOff (cost 1 -> 0) playable even at 0 energy!
        assert_eq!(f_no_adapt.playable_hand_count, 2);
        assert!(full_no_adapt.cost_zeroed_hand_bonus > 0);
        // Nightmare(DemonForm) boosts persistent buff setup beyond WasteAway penalty, and PaelsEye exempts exhausts.
        assert_eq!(full_no_adapt.wasted_engine_exhausts, 0);

        let mut s_adapt = s.clone();
        s_adapt.creatures[1].powers.push(PowerInst {
            handle: 204,
            id: PowerId::AdaptablePower,
            amount: 1,
            amount_on_turn_start: 1,
            skip_next_duration_tick: false,
            applier_handle: None,
            ext_index: None,
        });
        let (_f_adapt, full_adapt, _) =
            evaluate_fast_and_full_fused(&s_adapt, &content, &root, 0, 0, None, None);
        assert_eq!(full_adapt.poison_lethal_count, 0);
        assert!(full_adapt.poison_hp_reviving > 0);

        // Tactical signature is invariant to discard pile ordering!
        let mut s_disc_a = s.clone();
        s_disc_a.player.discard.push(s_disc_a.cards[0].handle);
        s_disc_a.player.discard.push(s_disc_a.cards[1].handle);
        let mut s_disc_b = s.clone();
        s_disc_b.player.discard.push(s_disc_b.cards[1].handle);
        s_disc_b.player.discard.push(s_disc_b.cards[0].handle);
        let (fa, fulla, _) =
            evaluate_fast_and_full_fused(&s_disc_a, &content, &root, 0, 0, None, None);
        let (fb, fullb, _) =
            evaluate_fast_and_full_fused(&s_disc_b, &content, &root, 0, 0, None, None);
        assert_eq!(fa.playable_hand_count, fb.playable_hand_count);
        assert_eq!(fulla.tactical_sig, fullb.tactical_sig);
        assert_ne!(fulla.tactical_sig, 0);
    }
}
