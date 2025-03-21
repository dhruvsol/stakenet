use std::{cmp::Ordering, collections::HashMap, mem::size_of, net::IpAddr};

use anchor_lang::prelude::*;
use borsh::{BorshDeserialize, BorshSerialize};
use type_layout::TypeLayout;

use crate::{
    crds_value::{ContactInfo, LegacyContactInfo, LegacyVersion, Version2},
    errors::ValidatorHistoryError,
    utils::cast_epoch,
};

static_assertions::const_assert_eq!(size_of::<Config>(), 104);

#[account]
#[derive(Default)]
pub struct Config {
    // This program is used to distribute MEV + track which validators are running jito-solana for a given epoch
    pub tip_distribution_program: Pubkey,

    // Has the ability to upgrade the tip_distribution_program in case of a program upgrade
    pub tip_distribution_authority: Pubkey,

    // Has the ability to publish stake amounts per validator
    pub stake_authority: Pubkey,

    // Tracks number of initialized ValidatorHistory accounts
    pub counter: u32,

    pub bump: u8,
}

impl Config {
    pub const SEED: &'static [u8] = b"config";
    pub const SIZE: usize = 8 + size_of::<Self>();
}

static_assertions::const_assert_eq!(size_of::<ValidatorHistoryEntry>(), 128);

#[derive(AnchorSerialize, TypeLayout)]
#[zero_copy]
pub struct ValidatorHistoryEntry {
    pub activated_stake_lamports: u64,
    pub epoch: u16,
    // MEV commission in basis points
    pub mev_commission: u16,
    // Number of successful votes in current epoch. Not finalized until subsequent epoch
    pub epoch_credits: u32,
    // Validator commission in points
    pub commission: u8,
    // 0 if Solana Labs client, 1 if Jito client, >1 if other
    pub client_type: u8,
    pub version: ClientVersion,
    pub ip: [u8; 4],
    // Required to keep 8-byte alignment
    pub padding0: u8,
    // 0 if not a superminority validator, 1 if superminority validator
    pub is_superminority: u8,
    // rank of validator by stake amount
    pub rank: u32,
    // Most recent updated slot for epoch credits and commission
    pub vote_account_last_update_slot: u64,
    pub padding1: [u8; 88],
}

impl Default for ValidatorHistoryEntry {
    fn default() -> Self {
        Self {
            activated_stake_lamports: u64::MAX,
            epoch: u16::MAX,
            mev_commission: u16::MAX,
            epoch_credits: u32::MAX,
            commission: u8::MAX,
            client_type: u8::MAX,
            version: ClientVersion {
                major: u8::MAX,
                minor: u8::MAX,
                patch: u16::MAX,
            },
            ip: [u8::MAX; 4],
            padding0: u8::MAX,
            is_superminority: u8::MAX,
            rank: u32::MAX,
            vote_account_last_update_slot: u64::MAX,
            padding1: [u8::MAX; 88],
        }
    }
}

#[derive(BorshSerialize, BorshDeserialize)]
#[zero_copy]
pub struct ClientVersion {
    pub major: u8,
    pub minor: u8,
    pub patch: u16,
}

const MAX_ITEMS: usize = 512;

#[derive(AnchorSerialize)]
#[zero_copy]
pub struct CircBuf {
    pub idx: u64,
    pub is_empty: u8,
    pub padding: [u8; 7],
    pub arr: [ValidatorHistoryEntry; MAX_ITEMS],
}

#[cfg(test)]
impl Default for CircBuf {
    fn default() -> Self {
        Self {
            arr: [ValidatorHistoryEntry::default(); MAX_ITEMS],
            idx: 0,
            is_empty: 1,
            padding: [0; 7],
        }
    }
}

impl CircBuf {
    pub fn push(&mut self, item: ValidatorHistoryEntry) {
        self.idx = (self.idx + 1) % self.arr.len() as u64;
        self.arr[self.idx as usize] = item;
        self.is_empty = 0;
    }

    pub fn is_empty(&self) -> bool {
        self.is_empty == 1
    }

    pub fn last(&self) -> Option<&ValidatorHistoryEntry> {
        if self.is_empty() {
            None
        } else {
            Some(&self.arr[self.idx as usize])
        }
    }

    pub fn last_mut(&mut self) -> Option<&mut ValidatorHistoryEntry> {
        if self.is_empty() {
            None
        } else {
            Some(&mut self.arr[self.idx as usize])
        }
    }

    pub fn arr_mut(&mut self) -> &mut [ValidatorHistoryEntry] {
        &mut self.arr
    }
}

pub enum ValidatorHistoryVersion {
    V0 = 0,
}

static_assertions::const_assert_eq!(size_of::<ValidatorHistory>(), 65848);

#[derive(AnchorSerialize)]
#[account(zero_copy)]
pub struct ValidatorHistory {
    // Cannot be enum due to Pod and Zeroable trait limitations
    pub struct_version: u32,

    pub vote_account: Pubkey,
    // Index of validator of all ValidatorHistory accounts
    pub index: u32,

    pub bump: u8,

    pub _padding0: [u8; 7],

    // These Crds gossip values are only signed and dated once upon startup and then never updated
    // so we track latest time on-chain to make sure old messages aren't uploaded
    pub last_ip_timestamp: u64,
    pub last_version_timestamp: u64,

    pub _padding1: [u8; 232],

    pub history: CircBuf,
}

impl ValidatorHistory {
    pub const SIZE: usize = 8 + size_of::<Self>();
    pub const MAX_ITEMS: usize = MAX_ITEMS;
    pub const SEED: &'static [u8] = b"validator-history";

    pub fn set_mev_commission(&mut self, epoch: u16, commission: u16) -> Result<()> {
        // check if entry exists for the epoch
        if let Some(entry) = self.history.last_mut() {
            if entry.epoch == epoch {
                entry.mev_commission = commission;
                return Ok(());
            }
        }
        let entry = ValidatorHistoryEntry {
            epoch,
            mev_commission: commission,
            ..ValidatorHistoryEntry::default()
        };
        self.history.push(entry);

        Ok(())
    }

    pub fn set_stake(
        &mut self,
        epoch: u16,
        stake: u64,
        rank: u32,
        is_superminority: bool,
    ) -> Result<()> {
        // Only one authority for upload here, so any epoch can be updated in case of missed upload
        if let Some(entry) = self.history.last_mut() {
            match entry.epoch.cmp(&epoch) {
                Ordering::Equal => {
                    entry.activated_stake_lamports = stake;
                    entry.rank = rank;
                    entry.is_superminority = is_superminority as u8;
                    return Ok(());
                }
                Ordering::Greater => {
                    for entry in self.history.arr_mut().iter_mut() {
                        if entry.epoch == epoch {
                            entry.activated_stake_lamports = stake;
                            entry.rank = rank;
                            entry.is_superminority = is_superminority as u8;
                            return Ok(());
                        }
                    }
                    return Err(ValidatorHistoryError::EpochOutOfRange.into());
                }
                Ordering::Less => {}
            }
        }
        let entry = ValidatorHistoryEntry {
            epoch,
            activated_stake_lamports: stake,
            rank,
            is_superminority: is_superminority as u8,
            ..ValidatorHistoryEntry::default()
        };
        self.history.push(entry);
        Ok(())
    }

    pub fn set_epoch_credits(
        &mut self,
        epoch_credits: &[(
            u64, /* epoch */
            u64, /* epoch cumulative votes */
            u64, /* prev epoch cumulative votes */
        )],
    ) -> Result<()> {
        // Assumes `set_commission` has already been run in `copy_vote_account`,
        // guaranteeing an entry exists for the current epoch
        if epoch_credits.is_empty() {
            return Ok(());
        }
        let epoch_credits_map: HashMap<u16, u32> =
            HashMap::from_iter(epoch_credits.iter().map(|(epoch, cur, prev)| {
                (
                    cast_epoch(*epoch),
                    (cur.checked_sub(*prev)
                        .ok_or(ValidatorHistoryError::InvalidEpochCredits)
                        .unwrap() as u32),
                )
            }));

        // Traverses entries in reverse order, breaking once we either:
        // 1) Start seeing identical epoch credit values
        // 2) See an epoch not in validator epoch credits (uninitialized or out of range)
        let len = self.history.arr.len();
        for i in 0..len {
            let position = (self.history.idx as usize + len - i) % len;
            let entry = &mut self.history.arr[position];
            if let Some(&epoch_credits) = epoch_credits_map.get(&entry.epoch) {
                if epoch_credits != entry.epoch_credits {
                    entry.epoch_credits = epoch_credits;
                } else {
                    break;
                }
            } else {
                break;
            }
        }

        Ok(())
    }

    pub fn set_commission_and_slot(&mut self, epoch: u16, commission: u8, slot: u64) -> Result<()> {
        if let Some(entry) = self.history.last_mut() {
            if entry.epoch == epoch {
                entry.commission = commission;
                entry.vote_account_last_update_slot = slot;
                return Ok(());
            }
        }
        let entry = ValidatorHistoryEntry {
            epoch,
            commission,
            vote_account_last_update_slot: slot,
            ..ValidatorHistoryEntry::default()
        };
        self.history.push(entry);

        Ok(())
    }

    pub fn set_contact_info(
        &mut self,
        epoch: u16,
        contact_info: &ContactInfo,
        contact_info_ts: u64,
    ) -> Result<()> {
        let ip = if let IpAddr::V4(address) = contact_info.addrs[0] {
            address.octets()
        } else {
            return Err(ValidatorHistoryError::UnsupportedIpFormat.into());
        };

        if self.last_ip_timestamp > contact_info_ts || self.last_version_timestamp > contact_info_ts
        {
            return Err(ValidatorHistoryError::GossipDataTooOld.into());
        }
        self.last_ip_timestamp = contact_info_ts;
        self.last_version_timestamp = contact_info_ts;

        if let Some(entry) = self.history.last_mut() {
            if entry.epoch == epoch {
                entry.ip = ip;
                entry.client_type = contact_info.version.client as u8;
                entry.version.major = contact_info.version.major as u8;
                entry.version.minor = contact_info.version.minor as u8;
                entry.version.patch = contact_info.version.patch;
                return Ok(());
            }
        }

        let entry = ValidatorHistoryEntry {
            epoch,
            ip,
            client_type: contact_info.version.client as u8,
            version: ClientVersion {
                major: contact_info.version.major as u8,
                minor: contact_info.version.minor as u8,
                patch: contact_info.version.patch,
            },
            ..ValidatorHistoryEntry::default()
        };
        self.history.push(entry);

        Ok(())
    }

    pub fn set_legacy_contact_info(
        &mut self,
        epoch: u16,
        legacy_contact_info: &LegacyContactInfo,
        contact_info_ts: u64,
    ) -> Result<()> {
        let ip = if let IpAddr::V4(address) = legacy_contact_info.gossip.ip() {
            address.octets()
        } else {
            return Err(ValidatorHistoryError::UnsupportedIpFormat.into());
        };
        if self.last_ip_timestamp > contact_info_ts {
            return Err(ValidatorHistoryError::GossipDataTooOld.into());
        }
        self.last_ip_timestamp = contact_info_ts;

        if let Some(entry) = self.history.last_mut() {
            if entry.epoch == epoch {
                entry.ip = ip;
                return Ok(());
            }
        }

        let entry = ValidatorHistoryEntry {
            epoch,
            ip,
            ..ValidatorHistoryEntry::default()
        };
        self.history.push(entry);
        Ok(())
    }

    pub fn set_version(&mut self, epoch: u16, version: &Version2, version_ts: u64) -> Result<()> {
        if self.last_version_timestamp > version_ts {
            return Err(ValidatorHistoryError::GossipDataTooOld.into());
        }
        self.last_version_timestamp = version_ts;

        if let Some(entry) = self.history.last_mut() {
            if entry.epoch == epoch {
                entry.version.major = version.version.major as u8;
                entry.version.minor = version.version.minor as u8;
                entry.version.patch = version.version.patch;
                return Ok(());
            }
        }
        let entry = ValidatorHistoryEntry {
            epoch,
            version: ClientVersion {
                major: version.version.major as u8,
                minor: version.version.minor as u8,
                patch: version.version.patch,
            },
            ..ValidatorHistoryEntry::default()
        };
        self.history.push(entry);
        Ok(())
    }

    pub fn set_legacy_version(
        &mut self,
        epoch: u16,
        legacy_version: &LegacyVersion,
        version_ts: u64,
    ) -> Result<()> {
        if self.last_version_timestamp > version_ts {
            return Err(ValidatorHistoryError::GossipDataTooOld.into());
        }
        self.last_version_timestamp = version_ts;

        if let Some(entry) = self.history.last_mut() {
            if entry.epoch == epoch {
                entry.version.major = legacy_version.version.major as u8;
                entry.version.minor = legacy_version.version.minor as u8;
                entry.version.patch = legacy_version.version.patch;
                return Ok(());
            }
        }
        let entry = ValidatorHistoryEntry {
            epoch,
            version: ClientVersion {
                major: legacy_version.version.major as u8,
                minor: legacy_version.version.minor as u8,
                patch: legacy_version.version.patch,
            },
            ..ValidatorHistoryEntry::default()
        };
        self.history.push(entry);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Utility test to see struct layout
    #[test]
    fn test_validator_history_layout() {
        println!("{}", ValidatorHistoryEntry::type_layout());
    }
}
