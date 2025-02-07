// Copyright 2022 CeresDB Project Authors. Licensed under Apache-2.0.

//! Adapter for the original datafusion repartiton optimization rule.

use std::sync::Arc;

use arrow_deps::datafusion::{
    physical_optimizer::{optimizer::PhysicalOptimizerRule, repartition::Repartition},
    physical_plan::ExecutionPlan,
    prelude::ExecutionConfig,
};
use log::debug;

use crate::physical_optimizer::{Adapter, OptimizeRuleRef};

pub struct RepartitionAdapter {
    original_rule: Repartition,
}

impl Default for RepartitionAdapter {
    fn default() -> Self {
        Self {
            original_rule: Repartition::new(),
        }
    }
}

impl Adapter for RepartitionAdapter {
    fn may_adapt(original_rule: OptimizeRuleRef) -> OptimizeRuleRef {
        if original_rule.name() == Repartition::new().name() {
            Arc::new(Self::default())
        } else {
            original_rule
        }
    }
}

impl PhysicalOptimizerRule for RepartitionAdapter {
    fn optimize(
        &self,
        plan: Arc<dyn ExecutionPlan>,
        config: &ExecutionConfig,
    ) -> arrow_deps::datafusion::error::Result<Arc<dyn ExecutionPlan>> {
        // the underlying plan maybe requires the order of the output.
        if plan.output_partitioning().partition_count() == 1 {
            debug!(
                "RepartitionAdapter avoid repartion optimization for plan:{:?}",
                plan
            );
            Ok(plan)
        } else {
            self.original_rule.optimize(plan, config)
        }
    }

    fn name(&self) -> &str {
        "custom-repartition"
    }
}
