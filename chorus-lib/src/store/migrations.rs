use super::Store;
use crate::error::Error;
use crate::types::Id;
use heed::byteorder::BigEndian;
use heed::types::{UnalignedSlice, Unit, U64};
use heed::RwTxn;

pub const CURRENT_MIGRATION_LEVEL: u32 = 5;

impl Store {
    pub fn migrate(&self) -> Result<(), Error> {
        let mut txn = self.env.write_txn()?;

        let mut migration_level = {
            let zero_bytes = 0_u32.to_be_bytes();
            let migration_level_bytes = self
                .general
                .get(&txn, b"migration_level")?
                .unwrap_or(zero_bytes.as_slice());
            u32::from_be_bytes(migration_level_bytes[..4].try_into().unwrap())
        };

        log::info!("Storage migration level = {}", migration_level);

        while migration_level < CURRENT_MIGRATION_LEVEL {
            self.migrate_to(&mut txn, migration_level + 1)?;
            migration_level += 1;
            self.general.put(
                &mut txn,
                b"migration_level",
                migration_level.to_be_bytes().as_slice(),
            )?;
        }

        txn.commit()?;

        Ok(())
    }

    fn migrate_to(&self, txn: &mut RwTxn<'_>, level: u32) -> Result<(), Error> {
        log::info!("Migrating database to {}", level);
        match level {
            1 => self.migrate_to_1(txn)?,
            2 => self.migrate_to_2(txn)?,
            3 => self.migrate_to_3(txn)?,
            4 => self.migrate_to_4(txn)?,
            5 => self.migrate_to_5(txn)?,
            _ => panic!("Unknown migration level {level}"),
        }

        Ok(())
    }

    // Populate ci_index
    fn migrate_to_1(&self, txn: &mut RwTxn<'_>) -> Result<(), Error> {
        let loop_txn = self.env.read_txn()?;
        let iter = self.i_index.iter(&loop_txn)?;
        for result in iter {
            let (_key, offset) = result?;
            let event = self.events.get_event_by_offset(offset)?;
            self.ci_index.put(
                txn,
                &Self::key_ci_index(event.created_at(), event.id()),
                &offset,
            )?;
        }

        Ok(())
    }

    // Populate tc_index and ac_index
    fn migrate_to_2(&self, txn: &mut RwTxn<'_>) -> Result<(), Error> {
        let loop_txn = self.env.read_txn()?;
        let iter = self.i_index.iter(&loop_txn)?;
        for result in iter {
            let (_key, offset) = result?;
            let event = self.events.get_event_by_offset(offset)?;

            // Add to ac_index
            self.ac_index.put(
                txn,
                &Self::key_ac_index(event.pubkey(), event.created_at(), event.id()),
                &offset,
            )?;

            // Add to tc_index
            for mut tsi in event.tags()?.iter() {
                if let Some(tagname) = tsi.next() {
                    if tagname.len() == 1 {
                        if let Some(tagvalue) = tsi.next() {
                            self.tc_index.put(
                                txn,
                                &Self::key_tc_index(
                                    tagname[0],
                                    tagvalue,
                                    event.created_at(),
                                    event.id(),
                                ),
                                &offset,
                            )?;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    // Clear IP data (we are hashing now)
    fn migrate_to_3(&self, txn: &mut RwTxn<'_>) -> Result<(), Error> {
        self.ip_data.clear(txn)?;
        Ok(())
    }

    // Clear deleted_offsets (now retired)
    fn migrate_to_4(&self, txn: &mut RwTxn<'_>) -> Result<(), Error> {
        let deleted_offsets = self
            .env
            .database_options()
            .types::<U64<BigEndian>, Unit>()
            .name("deleted_offsets")
            .create(txn)?;
        deleted_offsets.clear(txn)?;
        Ok(())
    }

    // Move data from deleted_events to deleted_ids
    fn migrate_to_5(&self, txn: &mut RwTxn<'_>) -> Result<(), Error> {
        let deleted_events = self
            .env
            .database_options()
            .types::<UnalignedSlice<u8>, Unit>()
            .name("deleted-events")
            .create(txn)?;

        let mut ids: Vec<Id> = Vec::new();

        for i in deleted_events.iter(txn)? {
            let (key, _val) = i?;
            let id = Id(key[0..32].try_into().unwrap());
            ids.push(id);
        }

        for id in ids.drain(..) {
            self.deleted_ids.put(txn, id.as_slice(), &())?;
        }

        deleted_events.clear(txn)?;

        Ok(())
    }
}
