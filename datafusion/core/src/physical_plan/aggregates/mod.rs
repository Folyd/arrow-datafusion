// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! Aggregates functionalities

use crate::execution::context::TaskContext;
use crate::physical_plan::aggregates::{
    bounded_aggregate_stream::BoundedAggregateStream, no_grouping::AggregateStream,
    row_hash::GroupedHashAggregateStream,
};
use crate::physical_plan::metrics::{
    BaselineMetrics, ExecutionPlanMetricsSet, MetricsSet,
};
use crate::physical_plan::{
    DisplayFormatType, Distribution, EquivalenceProperties, ExecutionPlan, Partitioning,
    SendableRecordBatchStream, Statistics,
};
use arrow::array::ArrayRef;
use arrow::datatypes::{Field, Schema, SchemaRef};
use arrow::record_batch::RecordBatch;
use datafusion_common::utils::longest_consecutive_prefix;
use datafusion_common::{DataFusionError, Result};
use datafusion_expr::Accumulator;
use datafusion_physical_expr::{
    aggregate::row_accumulator::RowAccumulator,
    equivalence::project_equivalence_properties,
    expressions::{Avg, CastExpr, Column, Sum},
    normalize_out_expr_with_columns_map,
    utils::{convert_to_expr, get_indices_of_matching_exprs, ordering_satisfy_concrete},
    AggregateExpr, OrderingEquivalenceProperties, PhysicalExpr, PhysicalSortExpr,
    PhysicalSortRequirement,
};
use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

mod bounded_aggregate_stream;
mod no_grouping;
mod row_hash;
mod utils;

pub use datafusion_expr::AggregateFunction;
pub use datafusion_physical_expr::expressions::create_aggregate_expr;
use datafusion_physical_expr::expressions::{ArrayAgg, FirstValue, LastValue};

/// Hash aggregate modes
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum AggregateMode {
    /// Partial aggregate that can be applied in parallel across input partitions
    Partial,
    /// Final aggregate that produces a single partition of output
    Final,
    /// Final aggregate that works on pre-partitioned data.
    ///
    /// This requires the invariant that all rows with a particular
    /// grouping key are in the same partitions, such as is the case
    /// with Hash repartitioning on the group keys. If a group key is
    /// duplicated, duplicate groups would be produced
    FinalPartitioned,
    /// Applies the entire logical aggregation operation in a single operator,
    /// as opposed to Partial / Final modes which apply the logical aggregation using
    /// two operators.
    Single,
}

/// Group By expression modes
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupByOrderMode {
    /// Some of the expressions in the GROUP BY clause have an ordering.
    // For example, if the input is ordered by a, b, c and we group by b, a, d;
    // the mode will be `PartiallyOrdered` meaning a subset of group b, a, d
    // defines a preset for the existing ordering, e.g a, b defines a preset.
    PartiallyOrdered,
    /// All the expressions in the GROUP BY clause have orderings.
    // For example, if the input is ordered by a, b, c, d and we group by b, a;
    // the mode will be `Ordered` meaning a all of the of group b, d
    // defines a preset for the existing ordering, e.g a, b defines a preset.
    FullyOrdered,
}

/// Represents `GROUP BY` clause in the plan (including the more general GROUPING SET)
/// In the case of a simple `GROUP BY a, b` clause, this will contain the expression [a, b]
/// and a single group [false, false].
/// In the case of `GROUP BY GROUPING SET/CUBE/ROLLUP` the planner will expand the expression
/// into multiple groups, using null expressions to align each group.
/// For example, with a group by clause `GROUP BY GROUPING SET ((a,b),(a),(b))` the planner should
/// create a `PhysicalGroupBy` like
/// PhysicalGroupBy {
///     expr: [(col(a), a), (col(b), b)],
///     null_expr: [(NULL, a), (NULL, b)],
///     groups: [
///         [false, false], // (a,b)
///         [false, true],  // (a) <=> (a, NULL)
///         [true, false]   // (b) <=> (NULL, b)
///     ]
/// }
#[derive(Clone, Debug, Default)]
pub struct PhysicalGroupBy {
    /// Distinct (Physical Expr, Alias) in the grouping set
    expr: Vec<(Arc<dyn PhysicalExpr>, String)>,
    /// Corresponding NULL expressions for expr
    null_expr: Vec<(Arc<dyn PhysicalExpr>, String)>,
    /// Null mask for each group in this grouping set. Each group is
    /// composed of either one of the group expressions in expr or a null
    /// expression in null_expr. If `groups[i][j]` is true, then the the
    /// j-th expression in the i-th group is NULL, otherwise it is `expr[j]`.
    groups: Vec<Vec<bool>>,
}

impl PhysicalGroupBy {
    /// Create a new `PhysicalGroupBy`
    pub fn new(
        expr: Vec<(Arc<dyn PhysicalExpr>, String)>,
        null_expr: Vec<(Arc<dyn PhysicalExpr>, String)>,
        groups: Vec<Vec<bool>>,
    ) -> Self {
        Self {
            expr,
            null_expr,
            groups,
        }
    }

    /// Create a GROUPING SET with only a single group. This is the "standard"
    /// case when building a plan from an expression such as `GROUP BY a,b,c`
    pub fn new_single(expr: Vec<(Arc<dyn PhysicalExpr>, String)>) -> Self {
        let num_exprs = expr.len();
        Self {
            expr,
            null_expr: vec![],
            groups: vec![vec![false; num_exprs]],
        }
    }

    /// Returns true if this GROUP BY contains NULL expressions
    pub fn contains_null(&self) -> bool {
        self.groups.iter().flatten().any(|is_null| *is_null)
    }

    /// Returns the group expressions
    pub fn expr(&self) -> &[(Arc<dyn PhysicalExpr>, String)] {
        &self.expr
    }

    /// Returns the null expressions
    pub fn null_expr(&self) -> &[(Arc<dyn PhysicalExpr>, String)] {
        &self.null_expr
    }

    /// Returns the group null masks
    pub fn groups(&self) -> &[Vec<bool>] {
        &self.groups
    }

    /// Returns true if this `PhysicalGroupBy` has no group expressions
    pub fn is_empty(&self) -> bool {
        self.expr.is_empty()
    }
}

impl PartialEq for PhysicalGroupBy {
    fn eq(&self, other: &PhysicalGroupBy) -> bool {
        self.expr.len() == other.expr.len()
            && self
                .expr
                .iter()
                .zip(other.expr.iter())
                .all(|((expr1, name1), (expr2, name2))| expr1.eq(expr2) && name1 == name2)
            && self.null_expr.len() == other.null_expr.len()
            && self
                .null_expr
                .iter()
                .zip(other.null_expr.iter())
                .all(|((expr1, name1), (expr2, name2))| expr1.eq(expr2) && name1 == name2)
            && self.groups == other.groups
    }
}

enum StreamType {
    AggregateStream(AggregateStream),
    GroupedHashAggregateStream(GroupedHashAggregateStream),
    BoundedAggregate(BoundedAggregateStream),
}

impl From<StreamType> for SendableRecordBatchStream {
    fn from(stream: StreamType) -> Self {
        match stream {
            StreamType::AggregateStream(stream) => Box::pin(stream),
            StreamType::GroupedHashAggregateStream(stream) => Box::pin(stream),
            StreamType::BoundedAggregate(stream) => Box::pin(stream),
        }
    }
}

/// This object encapsulates ordering-related information on GROUP BY columns.
#[derive(Debug, Clone)]
pub(crate) struct AggregationOrdering {
    /// Specifies whether the GROUP BY columns are partially or fully ordered.
    mode: GroupByOrderMode,
    /// Stores indices such that when we iterate with these indices, GROUP BY
    /// expressions match input ordering.
    order_indices: Vec<usize>,
    /// Actual ordering information of the GROUP BY columns.
    ordering: Vec<PhysicalSortExpr>,
}

/// Hash aggregate execution plan
#[derive(Debug)]
pub struct AggregateExec {
    /// Aggregation mode (full, partial)
    pub(crate) mode: AggregateMode,
    /// Group by expressions
    pub(crate) group_by: PhysicalGroupBy,
    /// Aggregate expressions
    pub(crate) aggr_expr: Vec<Arc<dyn AggregateExpr>>,
    /// FILTER (WHERE clause) expression for each aggregate expression
    pub(crate) filter_expr: Vec<Option<Arc<dyn PhysicalExpr>>>,
    /// (ORDER BY clause) expression for each aggregate expression
    pub(crate) order_by_expr: Vec<Option<Vec<PhysicalSortExpr>>>,
    /// Input plan, could be a partial aggregate or the input to the aggregate
    pub(crate) input: Arc<dyn ExecutionPlan>,
    /// Schema after the aggregate is applied
    schema: SchemaRef,
    /// Input schema before any aggregation is applied. For partial aggregate this will be the
    /// same as input.schema() but for the final aggregate it will be the same as the input
    /// to the partial aggregate
    pub(crate) input_schema: SchemaRef,
    /// The columns map used to normalize out expressions like Partitioning and PhysicalSortExpr
    /// The key is the column from the input schema and the values are the columns from the output schema
    columns_map: HashMap<Column, Vec<Column>>,
    /// Execution Metrics
    metrics: ExecutionPlanMetricsSet,
    /// Stores mode and output ordering information for the `AggregateExec`.
    aggregation_ordering: Option<AggregationOrdering>,
    required_input_ordering: Option<Vec<PhysicalSortRequirement>>,
}

/// Calculates the working mode for `GROUP BY` queries.
/// - If no GROUP BY expression has an ordering, returns `None`.
/// - If some GROUP BY expressions have an ordering, returns `Some(GroupByOrderMode::PartiallyOrdered)`.
/// - If all GROUP BY expressions have orderings, returns `Some(GroupByOrderMode::Ordered)`.
fn get_working_mode(
    input: &Arc<dyn ExecutionPlan>,
    group_by: &PhysicalGroupBy,
) -> Option<(GroupByOrderMode, Vec<usize>)> {
    if group_by.groups.len() > 1 {
        // We do not currently support streaming execution if we have more
        // than one group (e.g. we have grouping sets).
        return None;
    };

    let output_ordering = input.output_ordering().unwrap_or(&[]);
    // Since direction of the ordering is not important for GROUP BY columns,
    // we convert PhysicalSortExpr to PhysicalExpr in the existing ordering.
    let ordering_exprs = convert_to_expr(output_ordering);
    let groupby_exprs = group_by
        .expr
        .iter()
        .map(|(item, _)| item.clone())
        .collect::<Vec<_>>();
    // Find where each expression of the GROUP BY clause occurs in the existing
    // ordering (if it occurs):
    let mut ordered_indices =
        get_indices_of_matching_exprs(&groupby_exprs, &ordering_exprs, || {
            input.equivalence_properties()
        });
    ordered_indices.sort();
    // Find out how many expressions of the existing ordering define ordering
    // for expressions in the GROUP BY clause. For example, if the input is
    // ordered by a, b, c, d and we group by b, a, d; the result below would be.
    // 2, meaning 2 elements (a, b) among the GROUP BY columns define ordering.
    let first_n = longest_consecutive_prefix(ordered_indices);
    if first_n == 0 {
        // No GROUP by columns are ordered, we can not do streaming execution.
        return None;
    }
    let ordered_exprs = ordering_exprs[0..first_n].to_vec();
    // Find indices for the GROUP BY expressions such that when we iterate with
    // these indices, we would match existing ordering. For the example above,
    // this would produce 1, 0; meaning 1st and 0th entries (a, b) among the
    // GROUP BY expressions b, a, d match input ordering.
    let ordered_group_by_indices =
        get_indices_of_matching_exprs(&ordered_exprs, &groupby_exprs, || {
            input.equivalence_properties()
        });
    Some(if first_n == group_by.expr.len() {
        (GroupByOrderMode::FullyOrdered, ordered_group_by_indices)
    } else {
        (GroupByOrderMode::PartiallyOrdered, ordered_group_by_indices)
    })
}

/// This function gathers the ordering information for the GROUP BY columns.
fn calc_aggregation_ordering(
    input: &Arc<dyn ExecutionPlan>,
    group_by: &PhysicalGroupBy,
) -> Option<AggregationOrdering> {
    get_working_mode(input, group_by).map(|(mode, order_indices)| {
        let existing_ordering = input.output_ordering().unwrap_or(&[]);
        let out_group_expr = output_group_expr_helper(group_by);
        // Calculate output ordering information for the operator:
        let out_ordering = order_indices
            .iter()
            .zip(existing_ordering)
            .map(|(idx, input_col)| PhysicalSortExpr {
                expr: out_group_expr[*idx].clone(),
                options: input_col.options,
            })
            .collect::<Vec<_>>();
        AggregationOrdering {
            mode,
            order_indices,
            ordering: out_ordering,
        }
    })
}

/// This function returns grouping expressions as they occur in the output schema.
fn output_group_expr_helper(group_by: &PhysicalGroupBy) -> Vec<Arc<dyn PhysicalExpr>> {
    // Update column indices. Since the group by columns come first in the output schema, their
    // indices are simply 0..self.group_expr(len).
    group_by
        .expr()
        .iter()
        .enumerate()
        .map(|(index, (_, name))| Arc::new(Column::new(name, index)) as _)
        .collect()
}

/// This function gets the finest ordering requirement among all the aggregation
/// functions. If requirements are conflicting, (i.e. we can not compute the
/// aggregations in a single [`AggregateExec`]), the function returns an error.
fn get_finest_requirement<
    F: Fn() -> EquivalenceProperties,
    F2: Fn() -> OrderingEquivalenceProperties,
>(
    order_by_expr: &[Option<Vec<PhysicalSortExpr>>],
    eq_properties: F,
    ordering_eq_properties: F2,
) -> Result<Option<Vec<PhysicalSortExpr>>> {
    let mut result: Option<Vec<PhysicalSortExpr>> = None;
    for fn_reqs in order_by_expr.iter().flatten() {
        if let Some(result) = &mut result {
            if ordering_satisfy_concrete(
                result,
                fn_reqs,
                &eq_properties,
                &ordering_eq_properties,
            ) {
                // Do not update the result as it already satisfies current
                // function's requirement:
                continue;
            }
            if ordering_satisfy_concrete(
                fn_reqs,
                result,
                &eq_properties,
                &ordering_eq_properties,
            ) {
                // Update result with current function's requirements, as it is
                // a finer requirement than what we currently have.
                *result = fn_reqs.clone();
                continue;
            }
            // If neither of the requirements satisfy the other, this means
            // requirements are conflicting. Currently, we do not support
            // conflicting requirements.
            return Err(DataFusionError::NotImplemented(
                "Conflicting ordering requirements in aggregate functions is not supported".to_string(),
            ));
        } else {
            result = Some(fn_reqs.clone());
        }
    }
    Ok(result)
}

/// Checks whether the given aggregate expression is order-sensitive.
/// For instance, a `SUM` aggregation doesn't depend on the order of its inputs.
/// However, a `FirstAgg` depends on the input ordering (if the order changes,
/// the first value in the list would change).
fn is_order_sensitive(aggr_expr: &Arc<dyn AggregateExpr>) -> bool {
    aggr_expr.as_any().is::<FirstValue>()
        || aggr_expr.as_any().is::<LastValue>()
        || aggr_expr.as_any().is::<ArrayAgg>()
}

impl AggregateExec {
    /// Create a new hash aggregate execution plan
    pub fn try_new(
        mode: AggregateMode,
        group_by: PhysicalGroupBy,
        aggr_expr: Vec<Arc<dyn AggregateExpr>>,
        filter_expr: Vec<Option<Arc<dyn PhysicalExpr>>>,
        mut order_by_expr: Vec<Option<Vec<PhysicalSortExpr>>>,
        input: Arc<dyn ExecutionPlan>,
        input_schema: SchemaRef,
    ) -> Result<Self> {
        let schema = create_schema(
            &input.schema(),
            &group_by.expr,
            &aggr_expr,
            group_by.contains_null(),
            mode,
        )?;

        let schema = Arc::new(schema);
        let mut aggregator_requirement = None;
        // Ordering requirement makes sense only in Partial and Single modes.
        // In other modes, all groups are collapsed, therefore their input schema
        // can not contain expressions in the requirement.
        if mode == AggregateMode::Partial || mode == AggregateMode::Single {
            order_by_expr = aggr_expr
                .iter()
                .zip(order_by_expr.into_iter())
                .map(|(aggr_expr, fn_reqs)| {
                    // If aggregation function is ordering sensitive, keep ordering requirement as is; otherwise ignore requirement
                    if is_order_sensitive(aggr_expr) {
                        fn_reqs
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();
            let requirement = get_finest_requirement(
                &order_by_expr,
                || input.equivalence_properties(),
                || input.ordering_equivalence_properties(),
            )?;
            aggregator_requirement = requirement
                .map(|exprs| PhysicalSortRequirement::from_sort_exprs(exprs.iter()));
        }

        // construct a map from the input columns to the output columns of the Aggregation
        let mut columns_map: HashMap<Column, Vec<Column>> = HashMap::new();
        for (expression, name) in group_by.expr.iter() {
            if let Some(column) = expression.as_any().downcast_ref::<Column>() {
                let new_col_idx = schema.index_of(name)?;
                let entry = columns_map.entry(column.clone()).or_insert_with(Vec::new);
                entry.push(Column::new(name, new_col_idx));
            };
        }

        let aggregation_ordering = calc_aggregation_ordering(&input, &group_by);

        let mut required_input_ordering = None;
        if let Some(AggregationOrdering {
            ordering,
            // If the mode is FullyOrdered or PartiallyOrdered (i.e. we are
            // running with bounded memory, without breaking pipeline), then
            // we append aggregator ordering requirement to the existing
            // ordering. This way, we can still run with bounded memory.
            mode: GroupByOrderMode::FullyOrdered | GroupByOrderMode::PartiallyOrdered,
            ..
        }) = &aggregation_ordering
        {
            if let Some(aggregator_requirement) = aggregator_requirement {
                // Get the section of the input ordering that enables us to run in the
                // FullyOrdered or PartiallyOrdered mode:
                let requirement_prefix =
                    if let Some(existing_ordering) = input.output_ordering() {
                        existing_ordering[0..ordering.len()].to_vec()
                    } else {
                        vec![]
                    };
                let mut requirement =
                    PhysicalSortRequirement::from_sort_exprs(requirement_prefix.iter());
                for req in aggregator_requirement {
                    if requirement.iter().all(|item| req.expr.ne(&item.expr)) {
                        requirement.push(req);
                    }
                }
                required_input_ordering = Some(requirement);
            }
        } else {
            required_input_ordering = aggregator_requirement;
        }

        Ok(AggregateExec {
            mode,
            group_by,
            aggr_expr,
            filter_expr,
            order_by_expr,
            input,
            schema,
            input_schema,
            columns_map,
            metrics: ExecutionPlanMetricsSet::new(),
            aggregation_ordering,
            required_input_ordering,
        })
    }

    /// Aggregation mode (full, partial)
    pub fn mode(&self) -> &AggregateMode {
        &self.mode
    }

    /// Grouping expressions
    pub fn group_expr(&self) -> &PhysicalGroupBy {
        &self.group_by
    }

    /// Grouping expressions as they occur in the output schema
    pub fn output_group_expr(&self) -> Vec<Arc<dyn PhysicalExpr>> {
        output_group_expr_helper(&self.group_by)
    }

    /// Aggregate expressions
    pub fn aggr_expr(&self) -> &[Arc<dyn AggregateExpr>] {
        &self.aggr_expr
    }

    /// FILTER (WHERE clause) expression for each aggregate expression
    pub fn filter_expr(&self) -> &[Option<Arc<dyn PhysicalExpr>>] {
        &self.filter_expr
    }

    /// ORDER BY clause expression for each aggregate expression
    pub fn order_by_expr(&self) -> &[Option<Vec<PhysicalSortExpr>>] {
        &self.order_by_expr
    }

    /// Input plan
    pub fn input(&self) -> &Arc<dyn ExecutionPlan> {
        &self.input
    }

    /// Get the input schema before any aggregates are applied
    pub fn input_schema(&self) -> SchemaRef {
        self.input_schema.clone()
    }

    fn execute_typed(
        &self,
        partition: usize,
        context: Arc<TaskContext>,
    ) -> Result<StreamType> {
        let batch_size = context.session_config().batch_size();
        let scalar_update_factor = context.session_config().agg_scalar_update_factor();
        let input = self.input.execute(partition, Arc::clone(&context))?;
        let baseline_metrics = BaselineMetrics::new(&self.metrics, partition);

        if self.group_by.expr.is_empty() {
            Ok(StreamType::AggregateStream(AggregateStream::new(
                self.mode,
                self.schema.clone(),
                self.aggr_expr.clone(),
                self.filter_expr.clone(),
                input,
                baseline_metrics,
                context,
                partition,
            )?))
        } else if let Some(aggregation_ordering) = &self.aggregation_ordering {
            Ok(StreamType::BoundedAggregate(BoundedAggregateStream::new(
                self.mode,
                self.schema.clone(),
                self.group_by.clone(),
                self.aggr_expr.clone(),
                self.filter_expr.clone(),
                input,
                baseline_metrics,
                batch_size,
                scalar_update_factor,
                context,
                partition,
                aggregation_ordering.clone(),
            )?))
        } else {
            Ok(StreamType::GroupedHashAggregateStream(
                GroupedHashAggregateStream::new(
                    self.mode,
                    self.schema.clone(),
                    self.group_by.clone(),
                    self.aggr_expr.clone(),
                    self.filter_expr.clone(),
                    input,
                    baseline_metrics,
                    batch_size,
                    scalar_update_factor,
                    context,
                    partition,
                )?,
            ))
        }
    }
}

impl ExecutionPlan for AggregateExec {
    /// Return a reference to Any that can be used for down-casting
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    /// Get the output partitioning of this plan
    fn output_partitioning(&self) -> Partitioning {
        match &self.mode {
            AggregateMode::Partial | AggregateMode::Single => {
                // Partial and Single Aggregation will not change the output partitioning but need to respect the Alias
                let input_partition = self.input.output_partitioning();
                match input_partition {
                    Partitioning::Hash(exprs, part) => {
                        let normalized_exprs = exprs
                            .into_iter()
                            .map(|expr| {
                                normalize_out_expr_with_columns_map(
                                    expr,
                                    &self.columns_map,
                                )
                            })
                            .collect::<Vec<_>>();
                        Partitioning::Hash(normalized_exprs, part)
                    }
                    _ => input_partition,
                }
            }
            // Final Aggregation's output partitioning is the same as its real input
            _ => self.input.output_partitioning(),
        }
    }

    /// Specifies whether this plan generates an infinite stream of records.
    /// If the plan does not support pipelining, but its input(s) are
    /// infinite, returns an error to indicate this.
    fn unbounded_output(&self, children: &[bool]) -> Result<bool> {
        if children[0] {
            if self.aggregation_ordering.is_none() {
                // Cannot run without breaking pipeline.
                Err(DataFusionError::Plan(
                    "Aggregate Error: `GROUP BY` clauses with columns without ordering and GROUPING SETS are not supported for unbounded inputs.".to_string(),
                ))
            } else {
                Ok(true)
            }
        } else {
            Ok(false)
        }
    }

    fn output_ordering(&self) -> Option<&[PhysicalSortExpr]> {
        self.aggregation_ordering
            .as_ref()
            .map(|item: &AggregationOrdering| item.ordering.as_slice())
    }

    fn required_input_distribution(&self) -> Vec<Distribution> {
        match &self.mode {
            AggregateMode::Partial | AggregateMode::Single => {
                vec![Distribution::UnspecifiedDistribution]
            }
            AggregateMode::FinalPartitioned => {
                vec![Distribution::HashPartitioned(self.output_group_expr())]
            }
            AggregateMode::Final => vec![Distribution::SinglePartition],
        }
    }

    fn required_input_ordering(&self) -> Vec<Option<Vec<PhysicalSortRequirement>>> {
        vec![self.required_input_ordering.clone()]
    }

    fn equivalence_properties(&self) -> EquivalenceProperties {
        let mut new_properties = EquivalenceProperties::new(self.schema());
        project_equivalence_properties(
            self.input.equivalence_properties(),
            &self.columns_map,
            &mut new_properties,
        );
        new_properties
    }

    fn children(&self) -> Vec<Arc<dyn ExecutionPlan>> {
        vec![self.input.clone()]
    }

    fn with_new_children(
        self: Arc<Self>,
        children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        Ok(Arc::new(AggregateExec::try_new(
            self.mode,
            self.group_by.clone(),
            self.aggr_expr.clone(),
            self.filter_expr.clone(),
            self.order_by_expr.clone(),
            children[0].clone(),
            self.input_schema.clone(),
        )?))
    }

    fn execute(
        &self,
        partition: usize,
        context: Arc<TaskContext>,
    ) -> Result<SendableRecordBatchStream> {
        self.execute_typed(partition, context)
            .map(|stream| stream.into())
    }

    fn metrics(&self) -> Option<MetricsSet> {
        Some(self.metrics.clone_inner())
    }

    fn fmt_as(
        &self,
        t: DisplayFormatType,
        f: &mut std::fmt::Formatter,
    ) -> std::fmt::Result {
        match t {
            DisplayFormatType::Default => {
                write!(f, "AggregateExec: mode={:?}", self.mode)?;
                let g: Vec<String> = if self.group_by.groups.len() == 1 {
                    self.group_by
                        .expr
                        .iter()
                        .map(|(e, alias)| {
                            let e = e.to_string();
                            if &e != alias {
                                format!("{e} as {alias}")
                            } else {
                                e
                            }
                        })
                        .collect()
                } else {
                    self.group_by
                        .groups
                        .iter()
                        .map(|group| {
                            let terms = group
                                .iter()
                                .enumerate()
                                .map(|(idx, is_null)| {
                                    if *is_null {
                                        let (e, alias) = &self.group_by.null_expr[idx];
                                        let e = e.to_string();
                                        if &e != alias {
                                            format!("{e} as {alias}")
                                        } else {
                                            e
                                        }
                                    } else {
                                        let (e, alias) = &self.group_by.expr[idx];
                                        let e = e.to_string();
                                        if &e != alias {
                                            format!("{e} as {alias}")
                                        } else {
                                            e
                                        }
                                    }
                                })
                                .collect::<Vec<String>>()
                                .join(", ");
                            format!("({terms})")
                        })
                        .collect()
                };

                write!(f, ", gby=[{}]", g.join(", "))?;

                let a: Vec<String> = self
                    .aggr_expr
                    .iter()
                    .map(|agg| agg.name().to_string())
                    .collect();
                write!(f, ", aggr=[{}]", a.join(", "))?;

                if let Some(aggregation_ordering) = &self.aggregation_ordering {
                    write!(f, ", ordering_mode={:?}", aggregation_ordering.mode)?;
                }
            }
        }
        Ok(())
    }

    fn statistics(&self) -> Statistics {
        // TODO stats: group expressions:
        // - once expressions will be able to compute their own stats, use it here
        // - case where we group by on a column for which with have the `distinct` stat
        // TODO stats: aggr expression:
        // - aggregations somtimes also preserve invariants such as min, max...
        match self.mode {
            AggregateMode::Final | AggregateMode::FinalPartitioned
                if self.group_by.expr.is_empty() =>
            {
                Statistics {
                    num_rows: Some(1),
                    is_exact: true,
                    ..Default::default()
                }
            }
            _ => Statistics {
                // the output row count is surely not larger than its input row count
                num_rows: self.input.statistics().num_rows,
                is_exact: false,
                ..Default::default()
            },
        }
    }
}

fn create_schema(
    input_schema: &Schema,
    group_expr: &[(Arc<dyn PhysicalExpr>, String)],
    aggr_expr: &[Arc<dyn AggregateExpr>],
    contains_null_expr: bool,
    mode: AggregateMode,
) -> Result<Schema> {
    let mut fields = Vec::with_capacity(group_expr.len() + aggr_expr.len());
    for (expr, name) in group_expr {
        fields.push(Field::new(
            name,
            expr.data_type(input_schema)?,
            // In cases where we have multiple grouping sets, we will use NULL expressions in
            // order to align the grouping sets. So the field must be nullable even if the underlying
            // schema field is not.
            contains_null_expr || expr.nullable(input_schema)?,
        ))
    }

    match mode {
        AggregateMode::Partial => {
            // in partial mode, the fields of the accumulator's state
            for expr in aggr_expr {
                fields.extend(expr.state_fields()?.iter().cloned())
            }
        }
        AggregateMode::Final
        | AggregateMode::FinalPartitioned
        | AggregateMode::Single => {
            // in final mode, the field with the final result of the accumulator
            for expr in aggr_expr {
                fields.push(expr.field()?)
            }
        }
    }

    Ok(Schema::new(fields))
}

fn group_schema(schema: &Schema, group_count: usize) -> SchemaRef {
    let group_fields = schema.fields()[0..group_count].to_vec();
    Arc::new(Schema::new(group_fields))
}

/// returns physical expressions to evaluate against a batch
/// The expressions are different depending on `mode`:
/// * Partial: AggregateExpr::expressions
/// * Final: columns of `AggregateExpr::state_fields()`
fn aggregate_expressions(
    aggr_expr: &[Arc<dyn AggregateExpr>],
    mode: &AggregateMode,
    col_idx_base: usize,
) -> Result<Vec<Vec<Arc<dyn PhysicalExpr>>>> {
    match mode {
        AggregateMode::Partial | AggregateMode::Single => Ok(aggr_expr
            .iter()
            .map(|agg| {
                let pre_cast_type = if let Some(Sum {
                    data_type,
                    pre_cast_to_sum_type,
                    ..
                }) = agg.as_any().downcast_ref::<Sum>()
                {
                    if *pre_cast_to_sum_type {
                        Some(data_type.clone())
                    } else {
                        None
                    }
                } else if let Some(Avg {
                    sum_data_type,
                    pre_cast_to_sum_type,
                    ..
                }) = agg.as_any().downcast_ref::<Avg>()
                {
                    if *pre_cast_to_sum_type {
                        Some(sum_data_type.clone())
                    } else {
                        None
                    }
                } else {
                    None
                };
                agg.expressions()
                    .into_iter()
                    .map(|expr| {
                        pre_cast_type.clone().map_or(expr.clone(), |cast_type| {
                            Arc::new(CastExpr::new(expr, cast_type, None))
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .collect()),
        // in this mode, we build the merge expressions of the aggregation
        AggregateMode::Final | AggregateMode::FinalPartitioned => {
            let mut col_idx_base = col_idx_base;
            Ok(aggr_expr
                .iter()
                .map(|agg| {
                    let exprs = merge_expressions(col_idx_base, agg)?;
                    col_idx_base += exprs.len();
                    Ok(exprs)
                })
                .collect::<Result<Vec<_>>>()?)
        }
    }
}

/// uses `state_fields` to build a vec of physical column expressions required to merge the
/// AggregateExpr' accumulator's state.
///
/// `index_base` is the starting physical column index for the next expanded state field.
fn merge_expressions(
    index_base: usize,
    expr: &Arc<dyn AggregateExpr>,
) -> Result<Vec<Arc<dyn PhysicalExpr>>> {
    Ok(expr
        .state_fields()?
        .iter()
        .enumerate()
        .map(|(idx, f)| {
            Arc::new(Column::new(f.name(), index_base + idx)) as Arc<dyn PhysicalExpr>
        })
        .collect::<Vec<_>>())
}

pub(crate) type AccumulatorItem = Box<dyn Accumulator>;
pub(crate) type RowAccumulatorItem = Box<dyn RowAccumulator>;

fn create_accumulators(
    aggr_expr: &[Arc<dyn AggregateExpr>],
) -> Result<Vec<AccumulatorItem>> {
    aggr_expr
        .iter()
        .map(|expr| expr.create_accumulator())
        .collect::<Result<Vec<_>>>()
}

fn create_row_accumulators(
    aggr_expr: &[Arc<dyn AggregateExpr>],
) -> Result<Vec<RowAccumulatorItem>> {
    let mut state_index = 0;
    aggr_expr
        .iter()
        .map(|expr| {
            let result = expr.create_row_accumulator(state_index);
            state_index += expr.state_fields().unwrap().len();
            result
        })
        .collect::<Result<Vec<_>>>()
}

/// returns a vector of ArrayRefs, where each entry corresponds to either the
/// final value (mode = Final, FinalPartitioned and Single) or states (mode = Partial)
fn finalize_aggregation(
    accumulators: &[AccumulatorItem],
    mode: &AggregateMode,
) -> Result<Vec<ArrayRef>> {
    match mode {
        AggregateMode::Partial => {
            // build the vector of states
            let a = accumulators
                .iter()
                .map(|accumulator| accumulator.state())
                .map(|value| {
                    value.map(|e| {
                        e.iter().map(|v| v.to_array()).collect::<Vec<ArrayRef>>()
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(a.iter().flatten().cloned().collect::<Vec<_>>())
        }
        AggregateMode::Final
        | AggregateMode::FinalPartitioned
        | AggregateMode::Single => {
            // merge the state to the final value
            accumulators
                .iter()
                .map(|accumulator| accumulator.evaluate().map(|v| v.to_array()))
                .collect::<Result<Vec<ArrayRef>>>()
        }
    }
}

/// Evaluates expressions against a record batch.
fn evaluate(
    expr: &[Arc<dyn PhysicalExpr>],
    batch: &RecordBatch,
) -> Result<Vec<ArrayRef>> {
    expr.iter()
        .map(|expr| expr.evaluate(batch))
        .map(|r| r.map(|v| v.into_array(batch.num_rows())))
        .collect::<Result<Vec<_>>>()
}

/// Evaluates expressions against a record batch.
fn evaluate_many(
    expr: &[Vec<Arc<dyn PhysicalExpr>>],
    batch: &RecordBatch,
) -> Result<Vec<Vec<ArrayRef>>> {
    expr.iter()
        .map(|expr| evaluate(expr, batch))
        .collect::<Result<Vec<_>>>()
}

fn evaluate_optional(
    expr: &[Option<Arc<dyn PhysicalExpr>>],
    batch: &RecordBatch,
) -> Result<Vec<Option<ArrayRef>>> {
    expr.iter()
        .map(|expr| {
            expr.as_ref()
                .map(|expr| expr.evaluate(batch))
                .transpose()
                .map(|r| r.map(|v| v.into_array(batch.num_rows())))
        })
        .collect::<Result<Vec<_>>>()
}

fn evaluate_group_by(
    group_by: &PhysicalGroupBy,
    batch: &RecordBatch,
) -> Result<Vec<Vec<ArrayRef>>> {
    let exprs: Vec<ArrayRef> = group_by
        .expr
        .iter()
        .map(|(expr, _)| {
            let value = expr.evaluate(batch)?;
            Ok(value.into_array(batch.num_rows()))
        })
        .collect::<Result<Vec<_>>>()?;

    let null_exprs: Vec<ArrayRef> = group_by
        .null_expr
        .iter()
        .map(|(expr, _)| {
            let value = expr.evaluate(batch)?;
            Ok(value.into_array(batch.num_rows()))
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(group_by
        .groups
        .iter()
        .map(|group| {
            group
                .iter()
                .enumerate()
                .map(|(idx, is_null)| {
                    if *is_null {
                        null_exprs[idx].clone()
                    } else {
                        exprs[idx].clone()
                    }
                })
                .collect()
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use crate::execution::context::{SessionConfig, TaskContext};
    use crate::execution::runtime_env::{RuntimeConfig, RuntimeEnv};
    use crate::from_slice::FromSlice;
    use crate::physical_plan::aggregates::{
        get_finest_requirement, get_working_mode, AggregateExec, AggregateMode,
        PhysicalGroupBy,
    };
    use crate::physical_plan::expressions::{col, Avg};
    use crate::test::exec::{assert_strong_count_converges_to_zero, BlockingExec};
    use crate::test::{assert_is_pending, csv_exec_sorted};
    use crate::{assert_batches_sorted_eq, physical_plan::common};
    use arrow::array::{Float64Array, UInt32Array};
    use arrow::compute::{concat_batches, SortOptions};
    use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
    use arrow::record_batch::RecordBatch;
    use datafusion_common::{DataFusionError, Result, ScalarValue};
    use datafusion_physical_expr::expressions::{
        lit, ApproxDistinct, Column, Count, Median,
    };
    use datafusion_physical_expr::{
        AggregateExpr, EquivalenceProperties, OrderedColumn,
        OrderingEquivalenceProperties, PhysicalExpr, PhysicalSortExpr,
    };
    use futures::{FutureExt, Stream};
    use std::any::Any;
    use std::sync::Arc;
    use std::task::{Context, Poll};

    use super::StreamType;
    use crate::physical_plan::aggregates::GroupByOrderMode::{
        FullyOrdered, PartiallyOrdered,
    };
    use crate::physical_plan::coalesce_partitions::CoalescePartitionsExec;
    use crate::physical_plan::{
        ExecutionPlan, Partitioning, RecordBatchStream, SendableRecordBatchStream,
        Statistics,
    };
    use crate::prelude::SessionContext;

    // Generate a schema which consists of 5 columns (a, b, c, d, e)
    fn create_test_schema() -> Result<SchemaRef> {
        let a = Field::new("a", DataType::Int32, true);
        let b = Field::new("b", DataType::Int32, true);
        let c = Field::new("c", DataType::Int32, true);
        let d = Field::new("d", DataType::Int32, true);
        let e = Field::new("e", DataType::Int32, true);
        let schema = Arc::new(Schema::new(vec![a, b, c, d, e]));

        Ok(schema)
    }

    /// make PhysicalSortExpr with default options
    fn sort_expr(name: &str, schema: &Schema) -> PhysicalSortExpr {
        sort_expr_options(name, schema, SortOptions::default())
    }

    /// PhysicalSortExpr with specified options
    fn sort_expr_options(
        name: &str,
        schema: &Schema,
        options: SortOptions,
    ) -> PhysicalSortExpr {
        PhysicalSortExpr {
            expr: col(name, schema).unwrap(),
            options,
        }
    }

    #[tokio::test]
    async fn test_get_working_mode() -> Result<()> {
        let test_schema = create_test_schema()?;
        // Source is sorted by a ASC NULLS FIRST, b ASC NULLS FIRST, c ASC NULLS FIRST
        // Column d, e is not ordered.
        let sort_exprs = vec![
            sort_expr("a", &test_schema),
            sort_expr("b", &test_schema),
            sort_expr("c", &test_schema),
        ];
        let input = csv_exec_sorted(&test_schema, sort_exprs, true);

        // test cases consists of vector of tuples. Where each tuple represents a single test case.
        // First field in the tuple is Vec<str> where each element in the vector represents GROUP BY columns
        // For instance `vec!["a", "b"]` corresponds to GROUP BY a, b
        // Second field in the tuple is Option<GroupByOrderMode>, which corresponds to expected algorithm mode.
        // None represents that existing ordering is not sufficient to run executor with any one of the algorithms
        // (We need to add SortExec to be able to run it).
        // Some(GroupByOrderMode) represents, we can run algorithm with existing ordering; and algorithm should work in
        // GroupByOrderMode.
        let test_cases = vec![
            (vec!["a"], Some((FullyOrdered, vec![0]))),
            (vec!["b"], None),
            (vec!["c"], None),
            (vec!["b", "a"], Some((FullyOrdered, vec![1, 0]))),
            (vec!["c", "b"], None),
            (vec!["c", "a"], Some((PartiallyOrdered, vec![1]))),
            (vec!["c", "b", "a"], Some((FullyOrdered, vec![2, 1, 0]))),
            (vec!["d", "a"], Some((PartiallyOrdered, vec![1]))),
            (vec!["d", "b"], None),
            (vec!["d", "c"], None),
            (vec!["d", "b", "a"], Some((PartiallyOrdered, vec![2, 1]))),
            (vec!["d", "c", "b"], None),
            (vec!["d", "c", "a"], Some((PartiallyOrdered, vec![2]))),
            (
                vec!["d", "c", "b", "a"],
                Some((PartiallyOrdered, vec![3, 2, 1])),
            ),
        ];
        for (case_idx, test_case) in test_cases.iter().enumerate() {
            let (group_by_columns, expected) = &test_case;
            let mut group_by_exprs = vec![];
            for col_name in group_by_columns {
                group_by_exprs.push((col(col_name, &test_schema)?, col_name.to_string()));
            }
            let group_bys = PhysicalGroupBy::new_single(group_by_exprs);
            let res = get_working_mode(&input, &group_bys);
            assert_eq!(
                res, *expected,
                "Unexpected result for in unbounded test case#: {case_idx:?}, case: {test_case:?}"
            );
        }

        Ok(())
    }

    /// some mock data to aggregates
    fn some_data() -> (Arc<Schema>, Vec<RecordBatch>) {
        // define a schema.
        let schema = Arc::new(Schema::new(vec![
            Field::new("a", DataType::UInt32, false),
            Field::new("b", DataType::Float64, false),
        ]));

        // define data.
        (
            schema.clone(),
            vec![
                RecordBatch::try_new(
                    schema.clone(),
                    vec![
                        Arc::new(UInt32Array::from_slice([2, 3, 4, 4])),
                        Arc::new(Float64Array::from_slice([1.0, 2.0, 3.0, 4.0])),
                    ],
                )
                .unwrap(),
                RecordBatch::try_new(
                    schema,
                    vec![
                        Arc::new(UInt32Array::from_slice([2, 3, 3, 4])),
                        Arc::new(Float64Array::from_slice([1.0, 2.0, 3.0, 4.0])),
                    ],
                )
                .unwrap(),
            ],
        )
    }

    async fn check_grouping_sets(input: Arc<dyn ExecutionPlan>) -> Result<()> {
        let input_schema = input.schema();

        let grouping_set = PhysicalGroupBy {
            expr: vec![
                (col("a", &input_schema)?, "a".to_string()),
                (col("b", &input_schema)?, "b".to_string()),
            ],
            null_expr: vec![
                (lit(ScalarValue::UInt32(None)), "a".to_string()),
                (lit(ScalarValue::Float64(None)), "b".to_string()),
            ],
            groups: vec![
                vec![false, true],  // (a, NULL)
                vec![true, false],  // (NULL, b)
                vec![false, false], // (a,b)
            ],
        };

        let aggregates: Vec<Arc<dyn AggregateExpr>> = vec![Arc::new(Count::new(
            lit(1i8),
            "COUNT(1)".to_string(),
            DataType::Int64,
        ))];

        let session_ctx = SessionContext::new();
        let task_ctx = session_ctx.task_ctx();

        let partial_aggregate = Arc::new(AggregateExec::try_new(
            AggregateMode::Partial,
            grouping_set.clone(),
            aggregates.clone(),
            vec![None],
            vec![None],
            input,
            input_schema.clone(),
        )?);

        let result =
            common::collect(partial_aggregate.execute(0, task_ctx.clone())?).await?;

        let expected = vec![
            "+---+-----+-----------------+",
            "| a | b   | COUNT(1)[count] |",
            "+---+-----+-----------------+",
            "|   | 1.0 | 2               |",
            "|   | 2.0 | 2               |",
            "|   | 3.0 | 2               |",
            "|   | 4.0 | 2               |",
            "| 2 |     | 2               |",
            "| 2 | 1.0 | 2               |",
            "| 3 |     | 3               |",
            "| 3 | 2.0 | 2               |",
            "| 3 | 3.0 | 1               |",
            "| 4 |     | 3               |",
            "| 4 | 3.0 | 1               |",
            "| 4 | 4.0 | 2               |",
            "+---+-----+-----------------+",
        ];
        assert_batches_sorted_eq!(expected, &result);

        let groups = partial_aggregate.group_expr().expr().to_vec();

        let merge = Arc::new(CoalescePartitionsExec::new(partial_aggregate));

        let final_group: Vec<(Arc<dyn PhysicalExpr>, String)> = groups
            .iter()
            .map(|(_expr, name)| Ok((col(name, &input_schema)?, name.clone())))
            .collect::<Result<_>>()?;

        let final_grouping_set = PhysicalGroupBy::new_single(final_group);

        let merged_aggregate = Arc::new(AggregateExec::try_new(
            AggregateMode::Final,
            final_grouping_set,
            aggregates,
            vec![None],
            vec![None],
            merge,
            input_schema,
        )?);

        let result =
            common::collect(merged_aggregate.execute(0, task_ctx.clone())?).await?;
        let batch = concat_batches(&result[0].schema(), &result)?;
        assert_eq!(batch.num_columns(), 3);
        assert_eq!(batch.num_rows(), 12);

        let expected = vec![
            "+---+-----+----------+",
            "| a | b   | COUNT(1) |",
            "+---+-----+----------+",
            "|   | 1.0 | 2        |",
            "|   | 2.0 | 2        |",
            "|   | 3.0 | 2        |",
            "|   | 4.0 | 2        |",
            "| 2 |     | 2        |",
            "| 2 | 1.0 | 2        |",
            "| 3 |     | 3        |",
            "| 3 | 2.0 | 2        |",
            "| 3 | 3.0 | 1        |",
            "| 4 |     | 3        |",
            "| 4 | 3.0 | 1        |",
            "| 4 | 4.0 | 2        |",
            "+---+-----+----------+",
        ];

        assert_batches_sorted_eq!(&expected, &result);

        let metrics = merged_aggregate.metrics().unwrap();
        let output_rows = metrics.output_rows().unwrap();
        assert_eq!(12, output_rows);

        Ok(())
    }

    /// build the aggregates on the data from some_data() and check the results
    async fn check_aggregates(input: Arc<dyn ExecutionPlan>) -> Result<()> {
        let input_schema = input.schema();

        let grouping_set = PhysicalGroupBy {
            expr: vec![(col("a", &input_schema)?, "a".to_string())],
            null_expr: vec![],
            groups: vec![vec![false]],
        };

        let aggregates: Vec<Arc<dyn AggregateExpr>> = vec![Arc::new(Avg::new(
            col("b", &input_schema)?,
            "AVG(b)".to_string(),
            DataType::Float64,
        ))];

        let session_ctx = SessionContext::new();
        let task_ctx = session_ctx.task_ctx();

        let partial_aggregate = Arc::new(AggregateExec::try_new(
            AggregateMode::Partial,
            grouping_set.clone(),
            aggregates.clone(),
            vec![None],
            vec![None],
            input,
            input_schema.clone(),
        )?);

        let result =
            common::collect(partial_aggregate.execute(0, task_ctx.clone())?).await?;

        let expected = vec![
            "+---+---------------+-------------+",
            "| a | AVG(b)[count] | AVG(b)[sum] |",
            "+---+---------------+-------------+",
            "| 2 | 2             | 2.0         |",
            "| 3 | 3             | 7.0         |",
            "| 4 | 3             | 11.0        |",
            "+---+---------------+-------------+",
        ];
        assert_batches_sorted_eq!(expected, &result);

        let merge = Arc::new(CoalescePartitionsExec::new(partial_aggregate));

        let final_group: Vec<(Arc<dyn PhysicalExpr>, String)> = grouping_set
            .expr
            .iter()
            .map(|(_expr, name)| Ok((col(name, &input_schema)?, name.clone())))
            .collect::<Result<_>>()?;

        let final_grouping_set = PhysicalGroupBy::new_single(final_group);

        let merged_aggregate = Arc::new(AggregateExec::try_new(
            AggregateMode::Final,
            final_grouping_set,
            aggregates,
            vec![None],
            vec![None],
            merge,
            input_schema,
        )?);

        let result =
            common::collect(merged_aggregate.execute(0, task_ctx.clone())?).await?;
        let batch = concat_batches(&result[0].schema(), &result)?;
        assert_eq!(batch.num_columns(), 2);
        assert_eq!(batch.num_rows(), 3);

        let expected = vec![
            "+---+--------------------+",
            "| a | AVG(b)             |",
            "+---+--------------------+",
            "| 2 | 1.0                |",
            "| 3 | 2.3333333333333335 |", // 3, (2 + 3 + 2) / 3
            "| 4 | 3.6666666666666665 |", // 4, (3 + 4 + 4) / 3
            "+---+--------------------+",
        ];

        assert_batches_sorted_eq!(&expected, &result);

        let metrics = merged_aggregate.metrics().unwrap();
        let output_rows = metrics.output_rows().unwrap();
        assert_eq!(3, output_rows);

        Ok(())
    }

    /// Define a test source that can yield back to runtime before returning its first item ///

    #[derive(Debug)]
    struct TestYieldingExec {
        /// True if this exec should yield back to runtime the first time it is polled
        pub yield_first: bool,
    }

    impl ExecutionPlan for TestYieldingExec {
        fn as_any(&self) -> &dyn Any {
            self
        }
        fn schema(&self) -> SchemaRef {
            some_data().0
        }

        fn output_partitioning(&self) -> Partitioning {
            Partitioning::UnknownPartitioning(1)
        }

        fn output_ordering(&self) -> Option<&[PhysicalSortExpr]> {
            None
        }

        fn children(&self) -> Vec<Arc<dyn ExecutionPlan>> {
            vec![]
        }

        fn with_new_children(
            self: Arc<Self>,
            _: Vec<Arc<dyn ExecutionPlan>>,
        ) -> Result<Arc<dyn ExecutionPlan>> {
            Err(DataFusionError::Internal(format!(
                "Children cannot be replaced in {self:?}"
            )))
        }

        fn execute(
            &self,
            _partition: usize,
            _context: Arc<TaskContext>,
        ) -> Result<SendableRecordBatchStream> {
            let stream = if self.yield_first {
                TestYieldingStream::New
            } else {
                TestYieldingStream::Yielded
            };

            Ok(Box::pin(stream))
        }

        fn statistics(&self) -> Statistics {
            let (_, batches) = some_data();
            common::compute_record_batch_statistics(&[batches], &self.schema(), None)
        }
    }

    /// A stream using the demo data. If inited as new, it will first yield to runtime before returning records
    enum TestYieldingStream {
        New,
        Yielded,
        ReturnedBatch1,
        ReturnedBatch2,
    }

    impl Stream for TestYieldingStream {
        type Item = Result<RecordBatch>;

        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Option<Self::Item>> {
            match &*self {
                TestYieldingStream::New => {
                    *(self.as_mut()) = TestYieldingStream::Yielded;
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
                TestYieldingStream::Yielded => {
                    *(self.as_mut()) = TestYieldingStream::ReturnedBatch1;
                    Poll::Ready(Some(Ok(some_data().1[0].clone())))
                }
                TestYieldingStream::ReturnedBatch1 => {
                    *(self.as_mut()) = TestYieldingStream::ReturnedBatch2;
                    Poll::Ready(Some(Ok(some_data().1[1].clone())))
                }
                TestYieldingStream::ReturnedBatch2 => Poll::Ready(None),
            }
        }
    }

    impl RecordBatchStream for TestYieldingStream {
        fn schema(&self) -> SchemaRef {
            some_data().0
        }
    }

    //// Tests ////

    #[tokio::test]
    async fn aggregate_source_not_yielding() -> Result<()> {
        let input: Arc<dyn ExecutionPlan> =
            Arc::new(TestYieldingExec { yield_first: false });

        check_aggregates(input).await
    }

    #[tokio::test]
    async fn aggregate_grouping_sets_source_not_yielding() -> Result<()> {
        let input: Arc<dyn ExecutionPlan> =
            Arc::new(TestYieldingExec { yield_first: false });

        check_grouping_sets(input).await
    }

    #[tokio::test]
    async fn aggregate_source_with_yielding() -> Result<()> {
        let input: Arc<dyn ExecutionPlan> =
            Arc::new(TestYieldingExec { yield_first: true });

        check_aggregates(input).await
    }

    #[tokio::test]
    async fn aggregate_grouping_sets_with_yielding() -> Result<()> {
        let input: Arc<dyn ExecutionPlan> =
            Arc::new(TestYieldingExec { yield_first: true });

        check_grouping_sets(input).await
    }

    #[tokio::test]
    async fn test_oom() -> Result<()> {
        let input: Arc<dyn ExecutionPlan> =
            Arc::new(TestYieldingExec { yield_first: true });
        let input_schema = input.schema();

        let session_ctx = SessionContext::with_config_rt(
            SessionConfig::default(),
            Arc::new(
                RuntimeEnv::new(RuntimeConfig::default().with_memory_limit(1, 1.0))
                    .unwrap(),
            ),
        );
        let task_ctx = session_ctx.task_ctx();

        let groups_none = PhysicalGroupBy::default();
        let groups_some = PhysicalGroupBy {
            expr: vec![(col("a", &input_schema)?, "a".to_string())],
            null_expr: vec![],
            groups: vec![vec![false]],
        };

        // something that allocates within the aggregator
        let aggregates_v0: Vec<Arc<dyn AggregateExpr>> = vec![Arc::new(Median::new(
            col("a", &input_schema)?,
            "MEDIAN(a)".to_string(),
            DataType::UInt32,
        ))];

        // use slow-path in `hash.rs`
        let aggregates_v1: Vec<Arc<dyn AggregateExpr>> =
            vec![Arc::new(ApproxDistinct::new(
                col("a", &input_schema)?,
                "APPROX_DISTINCT(a)".to_string(),
                DataType::UInt32,
            ))];

        // use fast-path in `row_hash.rs`.
        let aggregates_v2: Vec<Arc<dyn AggregateExpr>> = vec![Arc::new(Avg::new(
            col("b", &input_schema)?,
            "AVG(b)".to_string(),
            DataType::Float64,
        ))];

        for (version, groups, aggregates) in [
            (0, groups_none, aggregates_v0),
            (1, groups_some.clone(), aggregates_v1),
            (2, groups_some, aggregates_v2),
        ] {
            let partial_aggregate = Arc::new(AggregateExec::try_new(
                AggregateMode::Partial,
                groups,
                aggregates,
                vec![None; 3],
                vec![None; 3],
                input.clone(),
                input_schema.clone(),
            )?);

            let stream = partial_aggregate.execute_typed(0, task_ctx.clone())?;

            // ensure that we really got the version we wanted
            match version {
                0 => {
                    assert!(matches!(stream, StreamType::AggregateStream(_)));
                }
                1 => {
                    assert!(matches!(stream, StreamType::GroupedHashAggregateStream(_)));
                }
                2 => {
                    assert!(matches!(stream, StreamType::GroupedHashAggregateStream(_)));
                }
                _ => panic!("Unknown version: {version}"),
            }

            let stream: SendableRecordBatchStream = stream.into();
            let err = common::collect(stream).await.unwrap_err();

            // error root cause traversal is a bit complicated, see #4172.
            let err = err.find_root();
            assert!(
                matches!(err, DataFusionError::ResourcesExhausted(_)),
                "Wrong error type: {err}",
            );
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_drop_cancel_without_groups() -> Result<()> {
        let session_ctx = SessionContext::new();
        let task_ctx = session_ctx.task_ctx();
        let schema =
            Arc::new(Schema::new(vec![Field::new("a", DataType::Float32, true)]));

        let groups = PhysicalGroupBy::default();

        let aggregates: Vec<Arc<dyn AggregateExpr>> = vec![Arc::new(Avg::new(
            col("a", &schema)?,
            "AVG(a)".to_string(),
            DataType::Float64,
        ))];

        let blocking_exec = Arc::new(BlockingExec::new(Arc::clone(&schema), 1));
        let refs = blocking_exec.refs();
        let aggregate_exec = Arc::new(AggregateExec::try_new(
            AggregateMode::Partial,
            groups.clone(),
            aggregates.clone(),
            vec![None],
            vec![None],
            blocking_exec,
            schema,
        )?);

        let fut = crate::physical_plan::collect(aggregate_exec, task_ctx);
        let mut fut = fut.boxed();

        assert_is_pending(&mut fut);
        drop(fut);
        assert_strong_count_converges_to_zero(refs).await;

        Ok(())
    }

    #[tokio::test]
    async fn test_drop_cancel_with_groups() -> Result<()> {
        let session_ctx = SessionContext::new();
        let task_ctx = session_ctx.task_ctx();
        let schema = Arc::new(Schema::new(vec![
            Field::new("a", DataType::Float32, true),
            Field::new("b", DataType::Float32, true),
        ]));

        let groups =
            PhysicalGroupBy::new_single(vec![(col("a", &schema)?, "a".to_string())]);

        let aggregates: Vec<Arc<dyn AggregateExpr>> = vec![Arc::new(Avg::new(
            col("b", &schema)?,
            "AVG(b)".to_string(),
            DataType::Float64,
        ))];

        let blocking_exec = Arc::new(BlockingExec::new(Arc::clone(&schema), 1));
        let refs = blocking_exec.refs();
        let aggregate_exec = Arc::new(AggregateExec::try_new(
            AggregateMode::Partial,
            groups,
            aggregates.clone(),
            vec![None],
            vec![None],
            blocking_exec,
            schema,
        )?);

        let fut = crate::physical_plan::collect(aggregate_exec, task_ctx);
        let mut fut = fut.boxed();

        assert_is_pending(&mut fut);
        drop(fut);
        assert_strong_count_converges_to_zero(refs).await;

        Ok(())
    }

    #[tokio::test]
    async fn test_get_finest_requirements() -> Result<()> {
        let test_schema = create_test_schema()?;
        // Assume column a and b are aliases
        // Assume also that a ASC and c DESC describe the same global ordering for the table. (Since they are ordering equivalent).
        let options1 = SortOptions {
            descending: false,
            nulls_first: false,
        };
        let options2 = SortOptions {
            descending: true,
            nulls_first: true,
        };
        let mut eq_properties = EquivalenceProperties::new(test_schema.clone());
        let col_a = Column::new("a", 0);
        let col_b = Column::new("b", 1);
        let col_c = Column::new("c", 2);
        let col_d = Column::new("d", 3);
        eq_properties.add_equal_conditions((&col_a, &col_b));
        let mut ordering_eq_properties = OrderingEquivalenceProperties::new(test_schema);
        ordering_eq_properties.add_equal_conditions((
            &vec![OrderedColumn::new(col_a.clone(), options1)],
            &vec![OrderedColumn::new(col_c.clone(), options2)],
        ));

        let order_by_exprs = vec![
            None,
            Some(vec![PhysicalSortExpr {
                expr: Arc::new(col_a.clone()),
                options: options1,
            }]),
            Some(vec![PhysicalSortExpr {
                expr: Arc::new(col_b.clone()),
                options: options1,
            }]),
            Some(vec![PhysicalSortExpr {
                expr: Arc::new(col_c),
                options: options2,
            }]),
            Some(vec![
                PhysicalSortExpr {
                    expr: Arc::new(col_a),
                    options: options1,
                },
                PhysicalSortExpr {
                    expr: Arc::new(col_d),
                    options: options1,
                },
            ]),
        ];
        let res = get_finest_requirement(
            &order_by_exprs,
            || eq_properties.clone(),
            || ordering_eq_properties.clone(),
        )?;
        assert_eq!(res, order_by_exprs[4]);
        Ok(())
    }
}
