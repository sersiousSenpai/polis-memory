// SPDX-License-Identifier: Apache-2.0
//! Local model accounting contains token counts and seat/model identifiers,
//! never prompt bodies. Unreported usage remains explicitly unknown.
use polis_llm::{Usage, UsageSink};
use polis_store::PolisStore;
use std::sync::Arc;

pub struct LocalUsageSink(pub Arc<PolisStore>);
impl UsageSink for LocalUsageSink {
    fn book(&self, seat: &str, usage: &Usage) {
        let result = (|| -> rusqlite::Result<()> {
            let mut conn = self.0.conn();
            let tx = conn.transaction()?;
            tx.execute("INSERT INTO model_usage(recorded_at,seat,model,input_tokens,output_tokens,cache_read_tokens,cache_creation_tokens,usage_reported)
                VALUES(?1,?2,?3,?4,?5,?6,?7,?8)", rusqlite::params![polis_core::ledger::now_millis(),seat,usage.model,
                    usage.input_tokens,usage.output_tokens,usage.cache_read_tokens,usage.cache_creation_tokens,usage.total_tokens() > 0])?;
            tx.execute("DELETE FROM model_usage WHERE id NOT IN (SELECT id FROM model_usage ORDER BY id DESC LIMIT 10000)", [])?;
            tx.commit()
        })();
        if let Err(error) = result {
            tracing::warn!(%error, seat, "local usage accounting failed");
        }
    }
}
