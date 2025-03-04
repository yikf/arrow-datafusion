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

//! Defines the cross join plan for loading the left side of the cross join
//! and producing batches in parallel for the right partitions

use futures::{ready, StreamExt};
use futures::{Stream, TryStreamExt};
use std::{any::Any, sync::Arc, task::Poll};

use arrow::datatypes::{Schema, SchemaRef};
use arrow::record_batch::RecordBatch;

use crate::execution::context::TaskContext;
use crate::execution::memory_pool::MemoryConsumer;
use crate::physical_plan::common::{OperatorMemoryReservation, SharedMemoryReservation};
use crate::physical_plan::metrics::{ExecutionPlanMetricsSet, MetricsSet};
use crate::physical_plan::{
    coalesce_batches::concat_batches, coalesce_partitions::CoalescePartitionsExec,
    ColumnStatistics, DisplayFormatType, Distribution, EquivalenceProperties,
    ExecutionPlan, Partitioning, PhysicalSortExpr, RecordBatchStream,
    SendableRecordBatchStream, Statistics,
};
use crate::{error::Result, scalar::ScalarValue};
use async_trait::async_trait;
use datafusion_common::DataFusionError;
use parking_lot::Mutex;

use super::utils::{
    adjust_right_output_partitioning, cross_join_equivalence_properties,
    BuildProbeJoinMetrics, OnceAsync, OnceFut,
};

/// Data of the left side
type JoinLeftData = RecordBatch;

/// executes partitions in parallel and combines them into a set of
/// partitions by combining all values from the left with all values on the right
#[derive(Debug)]
pub struct CrossJoinExec {
    /// left (build) side which gets loaded in memory
    pub(crate) left: Arc<dyn ExecutionPlan>,
    /// right (probe) side which are combined with left side
    pub(crate) right: Arc<dyn ExecutionPlan>,
    /// The schema once the join is applied
    schema: SchemaRef,
    /// Build-side data
    left_fut: OnceAsync<JoinLeftData>,
    /// Memory reservation for build-side data
    reservation: OperatorMemoryReservation,
    /// Execution plan metrics
    metrics: ExecutionPlanMetricsSet,
}

impl CrossJoinExec {
    /// Create a new [CrossJoinExec].
    pub fn new(left: Arc<dyn ExecutionPlan>, right: Arc<dyn ExecutionPlan>) -> Self {
        // left then right
        let all_columns = {
            let left_schema = left.schema();
            let right_schema = right.schema();
            let left_fields = left_schema.fields().iter();
            let right_fields = right_schema.fields().iter();
            left_fields.chain(right_fields).cloned().collect()
        };

        let schema = Arc::new(Schema::new(all_columns));

        CrossJoinExec {
            left,
            right,
            schema,
            left_fut: Default::default(),
            reservation: Default::default(),
            metrics: ExecutionPlanMetricsSet::default(),
        }
    }

    /// left (build) side which gets loaded in memory
    pub fn left(&self) -> &Arc<dyn ExecutionPlan> {
        &self.left
    }

    /// right side which gets combined with left side
    pub fn right(&self) -> &Arc<dyn ExecutionPlan> {
        &self.right
    }
}

/// Asynchronously collect the result of the left child
async fn load_left_input(
    left: Arc<dyn ExecutionPlan>,
    context: Arc<TaskContext>,
    metrics: BuildProbeJoinMetrics,
    reservation: SharedMemoryReservation,
) -> Result<JoinLeftData> {
    // merge all left parts into a single stream
    let merge = {
        if left.output_partitioning().partition_count() != 1 {
            Arc::new(CoalescePartitionsExec::new(left.clone()))
        } else {
            left.clone()
        }
    };
    let stream = merge.execute(0, context)?;

    // Load all batches and count the rows
    let (batches, num_rows, _, _) = stream
        .try_fold(
            (Vec::new(), 0usize, metrics, reservation),
            |mut acc, batch| async {
                let batch_size = batch.get_array_memory_size();
                // Reserve memory for incoming batch
                acc.3.lock().try_grow(batch_size)?;
                // Update metrics
                acc.2.build_mem_used.add(batch_size);
                acc.2.build_input_batches.add(1);
                acc.2.build_input_rows.add(batch.num_rows());
                // Update rowcount
                acc.1 += batch.num_rows();
                // Push batch to output
                acc.0.push(batch);
                Ok(acc)
            },
        )
        .await?;

    let merged_batch = concat_batches(&left.schema(), &batches, num_rows)?;

    Ok(merged_batch)
}

impl ExecutionPlan for CrossJoinExec {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    fn children(&self) -> Vec<Arc<dyn ExecutionPlan>> {
        vec![self.left.clone(), self.right.clone()]
    }

    fn metrics(&self) -> Option<MetricsSet> {
        Some(self.metrics.clone_inner())
    }

    /// Specifies whether this plan generates an infinite stream of records.
    /// If the plan does not support pipelining, but it its input(s) are
    /// infinite, returns an error to indicate this.    
    fn unbounded_output(&self, children: &[bool]) -> Result<bool> {
        if children[0] || children[1] {
            Err(DataFusionError::Plan(
                "Cross Join Error: Cross join is not supported for the unbounded inputs."
                    .to_string(),
            ))
        } else {
            Ok(false)
        }
    }

    fn with_new_children(
        self: Arc<Self>,
        children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        Ok(Arc::new(CrossJoinExec::new(
            children[0].clone(),
            children[1].clone(),
        )))
    }

    fn required_input_distribution(&self) -> Vec<Distribution> {
        vec![
            Distribution::SinglePartition,
            Distribution::UnspecifiedDistribution,
        ]
    }

    // TODO optimize CrossJoin implementation to generate M * N partitions
    fn output_partitioning(&self) -> Partitioning {
        let left_columns_len = self.left.schema().fields.len();
        adjust_right_output_partitioning(
            self.right.output_partitioning(),
            left_columns_len,
        )
    }

    // TODO check the output ordering of CrossJoin
    fn output_ordering(&self) -> Option<&[PhysicalSortExpr]> {
        None
    }

    fn equivalence_properties(&self) -> EquivalenceProperties {
        let left_columns_len = self.left.schema().fields.len();
        cross_join_equivalence_properties(
            self.left.equivalence_properties(),
            self.right.equivalence_properties(),
            left_columns_len,
            self.schema(),
        )
    }

    fn execute(
        &self,
        partition: usize,
        context: Arc<TaskContext>,
    ) -> Result<SendableRecordBatchStream> {
        let stream = self.right.execute(partition, context.clone())?;

        let join_metrics = BuildProbeJoinMetrics::new(partition, &self.metrics);

        // Initialization of operator-level reservation
        {
            let mut reservation_lock = self.reservation.lock();
            if reservation_lock.is_none() {
                *reservation_lock = Some(Arc::new(Mutex::new(
                    MemoryConsumer::new("CrossJoinExec").register(context.memory_pool()),
                )));
            };
        }

        let reservation = self.reservation.lock().clone().ok_or_else(|| {
            DataFusionError::Internal(
                "Operator-level memory reservation is not initialized".to_string(),
            )
        })?;

        let left_fut = self.left_fut.once(|| {
            load_left_input(
                self.left.clone(),
                context,
                join_metrics.clone(),
                reservation,
            )
        });

        Ok(Box::pin(CrossJoinStream {
            schema: self.schema.clone(),
            left_fut,
            right: stream,
            right_batch: Arc::new(parking_lot::Mutex::new(None)),
            left_index: 0,
            join_metrics,
        }))
    }

    fn fmt_as(
        &self,
        t: DisplayFormatType,
        f: &mut std::fmt::Formatter,
    ) -> std::fmt::Result {
        match t {
            DisplayFormatType::Default => {
                write!(f, "CrossJoinExec")
            }
        }
    }

    fn statistics(&self) -> Statistics {
        stats_cartesian_product(
            self.left.statistics(),
            self.left.schema().fields().len(),
            self.right.statistics(),
            self.right.schema().fields().len(),
        )
    }
}

/// [left/right]_col_count are required in case the column statistics are None
fn stats_cartesian_product(
    left_stats: Statistics,
    left_col_count: usize,
    right_stats: Statistics,
    right_col_count: usize,
) -> Statistics {
    let left_row_count = left_stats.num_rows;
    let right_row_count = right_stats.num_rows;

    // calculate global stats
    let is_exact = left_stats.is_exact && right_stats.is_exact;
    let num_rows = left_stats
        .num_rows
        .zip(right_stats.num_rows)
        .map(|(a, b)| a * b);
    // the result size is two times a*b because you have the columns of both left and right
    let total_byte_size = left_stats
        .total_byte_size
        .zip(right_stats.total_byte_size)
        .map(|(a, b)| 2 * a * b);

    // calculate column stats
    let column_statistics =
        // complete the column statistics if they are missing only on one side
        match (left_stats.column_statistics, right_stats.column_statistics) {
            (None, None) => None,
            (None, Some(right_col_stat)) => Some((
                vec![ColumnStatistics::default(); left_col_count],
                right_col_stat,
            )),
            (Some(left_col_stat), None) => Some((
                left_col_stat,
                vec![ColumnStatistics::default(); right_col_count],
            )),
            (Some(left_col_stat), Some(right_col_stat)) => {
                Some((left_col_stat, right_col_stat))
            }
        }
        .map(|(left_col_stats, right_col_stats)| {
            // the null counts must be multiplied by the row counts of the other side (if defined)
            // Min, max and distinct_count on the other hand are invariants.
            left_col_stats.into_iter().map(|s| ColumnStatistics{
                null_count: s.null_count.zip(right_row_count).map(|(a, b)| a * b),
                distinct_count: s.distinct_count,
                min_value: s.min_value,
                max_value: s.max_value,
            }).chain(
            right_col_stats.into_iter().map(|s| ColumnStatistics{
                null_count: s.null_count.zip(left_row_count).map(|(a, b)| a * b),
                distinct_count: s.distinct_count,
                min_value: s.min_value,
                max_value: s.max_value,
            })).collect()
        });

    Statistics {
        is_exact,
        num_rows,
        total_byte_size,
        column_statistics,
    }
}

/// A stream that issues [RecordBatch]es as they arrive from the right  of the join.
struct CrossJoinStream {
    /// Input schema
    schema: Arc<Schema>,
    /// future for data from left side
    left_fut: OnceFut<JoinLeftData>,
    /// right
    right: SendableRecordBatchStream,
    /// Current value on the left
    left_index: usize,
    /// Current batch being processed from the right side
    right_batch: Arc<parking_lot::Mutex<Option<RecordBatch>>>,
    /// join execution metrics
    join_metrics: BuildProbeJoinMetrics,
}

impl RecordBatchStream for CrossJoinStream {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }
}

fn build_batch(
    left_index: usize,
    batch: &RecordBatch,
    left_data: &RecordBatch,
    schema: &Schema,
) -> Result<RecordBatch> {
    // Repeat value on the left n times
    let arrays = left_data
        .columns()
        .iter()
        .map(|arr| {
            let scalar = ScalarValue::try_from_array(arr, left_index)?;
            Ok(scalar.to_array_of_size(batch.num_rows()))
        })
        .collect::<Result<Vec<_>>>()?;

    RecordBatch::try_new(
        Arc::new(schema.clone()),
        arrays
            .iter()
            .chain(batch.columns().iter())
            .cloned()
            .collect(),
    )
    .map_err(Into::into)
}

#[async_trait]
impl Stream for CrossJoinStream {
    type Item = Result<RecordBatch>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.poll_next_impl(cx)
    }
}

impl CrossJoinStream {
    /// Separate implementation function that unpins the [`CrossJoinStream`] so
    /// that partial borrows work correctly
    fn poll_next_impl(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<RecordBatch>>> {
        let build_timer = self.join_metrics.build_time.timer();
        let left_data = match ready!(self.left_fut.get(cx)) {
            Ok(left_data) => left_data,
            Err(e) => return Poll::Ready(Some(Err(e))),
        };
        build_timer.done();

        if left_data.num_rows() == 0 {
            return Poll::Ready(None);
        }

        if self.left_index > 0 && self.left_index < left_data.num_rows() {
            let join_timer = self.join_metrics.join_time.timer();
            let right_batch = {
                let right_batch = self.right_batch.lock();
                right_batch.clone().unwrap()
            };
            let result =
                build_batch(self.left_index, &right_batch, left_data, &self.schema);
            self.join_metrics.input_rows.add(right_batch.num_rows());
            if let Ok(ref batch) = result {
                join_timer.done();
                self.join_metrics.output_batches.add(1);
                self.join_metrics.output_rows.add(batch.num_rows());
            }
            self.left_index += 1;
            return Poll::Ready(Some(result));
        }
        self.left_index = 0;
        self.right
            .poll_next_unpin(cx)
            .map(|maybe_batch| match maybe_batch {
                Some(Ok(batch)) => {
                    let join_timer = self.join_metrics.join_time.timer();
                    let result =
                        build_batch(self.left_index, &batch, left_data, &self.schema);
                    self.join_metrics.input_batches.add(1);
                    self.join_metrics.input_rows.add(batch.num_rows());
                    if let Ok(ref batch) = result {
                        join_timer.done();
                        self.join_metrics.output_batches.add(1);
                        self.join_metrics.output_rows.add(batch.num_rows());
                    }
                    self.left_index = 1;

                    let mut right_batch = self.right_batch.lock();
                    *right_batch = Some(batch);

                    Some(result)
                }
                other => other,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assert_batches_sorted_eq;
    use crate::common::assert_contains;
    use crate::execution::runtime_env::{RuntimeConfig, RuntimeEnv};
    use crate::physical_plan::common;
    use crate::prelude::{SessionConfig, SessionContext};
    use crate::test::{build_table_scan_i32, columns};

    async fn join_collect(
        left: Arc<dyn ExecutionPlan>,
        right: Arc<dyn ExecutionPlan>,
        context: Arc<TaskContext>,
    ) -> Result<(Vec<String>, Vec<RecordBatch>)> {
        let join = CrossJoinExec::new(left, right);
        let columns_header = columns(&join.schema());

        let stream = join.execute(0, context)?;
        let batches = common::collect(stream).await?;

        Ok((columns_header, batches))
    }

    #[tokio::test]
    async fn test_stats_cartesian_product() {
        let left_row_count = 11;
        let left_bytes = 23;
        let right_row_count = 7;
        let right_bytes = 27;

        let left = Statistics {
            is_exact: true,
            num_rows: Some(left_row_count),
            total_byte_size: Some(left_bytes),
            column_statistics: Some(vec![
                ColumnStatistics {
                    distinct_count: Some(5),
                    max_value: Some(ScalarValue::Int64(Some(21))),
                    min_value: Some(ScalarValue::Int64(Some(-4))),
                    null_count: Some(0),
                },
                ColumnStatistics {
                    distinct_count: Some(1),
                    max_value: Some(ScalarValue::Utf8(Some(String::from("x")))),
                    min_value: Some(ScalarValue::Utf8(Some(String::from("a")))),
                    null_count: Some(3),
                },
            ]),
        };

        let right = Statistics {
            is_exact: true,
            num_rows: Some(right_row_count),
            total_byte_size: Some(right_bytes),
            column_statistics: Some(vec![ColumnStatistics {
                distinct_count: Some(3),
                max_value: Some(ScalarValue::Int64(Some(12))),
                min_value: Some(ScalarValue::Int64(Some(0))),
                null_count: Some(2),
            }]),
        };

        let result = stats_cartesian_product(left, 3, right, 2);

        let expected = Statistics {
            is_exact: true,
            num_rows: Some(left_row_count * right_row_count),
            total_byte_size: Some(2 * left_bytes * right_bytes),
            column_statistics: Some(vec![
                ColumnStatistics {
                    distinct_count: Some(5),
                    max_value: Some(ScalarValue::Int64(Some(21))),
                    min_value: Some(ScalarValue::Int64(Some(-4))),
                    null_count: Some(0),
                },
                ColumnStatistics {
                    distinct_count: Some(1),
                    max_value: Some(ScalarValue::Utf8(Some(String::from("x")))),
                    min_value: Some(ScalarValue::Utf8(Some(String::from("a")))),
                    null_count: Some(3 * right_row_count),
                },
                ColumnStatistics {
                    distinct_count: Some(3),
                    max_value: Some(ScalarValue::Int64(Some(12))),
                    min_value: Some(ScalarValue::Int64(Some(0))),
                    null_count: Some(2 * left_row_count),
                },
            ]),
        };

        assert_eq!(result, expected);
    }

    #[tokio::test]
    async fn test_stats_cartesian_product_with_unknwon_size() {
        let left_row_count = 11;

        let left = Statistics {
            is_exact: true,
            num_rows: Some(left_row_count),
            total_byte_size: Some(23),
            column_statistics: Some(vec![
                ColumnStatistics {
                    distinct_count: Some(5),
                    max_value: Some(ScalarValue::Int64(Some(21))),
                    min_value: Some(ScalarValue::Int64(Some(-4))),
                    null_count: Some(0),
                },
                ColumnStatistics {
                    distinct_count: Some(1),
                    max_value: Some(ScalarValue::Utf8(Some(String::from("x")))),
                    min_value: Some(ScalarValue::Utf8(Some(String::from("a")))),
                    null_count: Some(3),
                },
            ]),
        };

        let right = Statistics {
            is_exact: true,
            num_rows: None,        // not defined!
            total_byte_size: None, // not defined!
            column_statistics: Some(vec![ColumnStatistics {
                distinct_count: Some(3),
                max_value: Some(ScalarValue::Int64(Some(12))),
                min_value: Some(ScalarValue::Int64(Some(0))),
                null_count: Some(2),
            }]),
        };

        let result = stats_cartesian_product(left, 3, right, 2);

        let expected = Statistics {
            is_exact: true,
            num_rows: None,
            total_byte_size: None,
            column_statistics: Some(vec![
                ColumnStatistics {
                    distinct_count: Some(5),
                    max_value: Some(ScalarValue::Int64(Some(21))),
                    min_value: Some(ScalarValue::Int64(Some(-4))),
                    null_count: None, // we don't know the row count on the right
                },
                ColumnStatistics {
                    distinct_count: Some(1),
                    max_value: Some(ScalarValue::Utf8(Some(String::from("x")))),
                    min_value: Some(ScalarValue::Utf8(Some(String::from("a")))),
                    null_count: None, // we don't know the row count on the right
                },
                ColumnStatistics {
                    distinct_count: Some(3),
                    max_value: Some(ScalarValue::Int64(Some(12))),
                    min_value: Some(ScalarValue::Int64(Some(0))),
                    null_count: Some(2 * left_row_count),
                },
            ]),
        };

        assert_eq!(result, expected);
    }

    #[tokio::test]
    async fn test_join() -> Result<()> {
        let session_ctx = SessionContext::new();
        let task_ctx = session_ctx.task_ctx();

        let left = build_table_scan_i32(
            ("a1", &vec![1, 2, 3]),
            ("b1", &vec![4, 5, 6]),
            ("c1", &vec![7, 8, 9]),
        );
        let right = build_table_scan_i32(
            ("a2", &vec![10, 11]),
            ("b2", &vec![12, 13]),
            ("c2", &vec![14, 15]),
        );

        let (columns, batches) = join_collect(left, right, task_ctx).await?;

        assert_eq!(columns, vec!["a1", "b1", "c1", "a2", "b2", "c2"]);
        let expected = vec![
            "+----+----+----+----+----+----+",
            "| a1 | b1 | c1 | a2 | b2 | c2 |",
            "+----+----+----+----+----+----+",
            "| 1  | 4  | 7  | 10 | 12 | 14 |",
            "| 1  | 4  | 7  | 11 | 13 | 15 |",
            "| 2  | 5  | 8  | 10 | 12 | 14 |",
            "| 2  | 5  | 8  | 11 | 13 | 15 |",
            "| 3  | 6  | 9  | 10 | 12 | 14 |",
            "| 3  | 6  | 9  | 11 | 13 | 15 |",
            "+----+----+----+----+----+----+",
        ];

        assert_batches_sorted_eq!(expected, &batches);

        Ok(())
    }

    #[tokio::test]
    async fn test_overallocation() -> Result<()> {
        let runtime_config = RuntimeConfig::new().with_memory_limit(100, 1.0);
        let runtime = Arc::new(RuntimeEnv::new(runtime_config)?);
        let session_ctx =
            SessionContext::with_config_rt(SessionConfig::default(), runtime);
        let task_ctx = session_ctx.task_ctx();

        let left = build_table_scan_i32(
            ("a1", &vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 0]),
            ("b1", &vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 0]),
            ("c1", &vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 0]),
        );
        let right = build_table_scan_i32(
            ("a2", &vec![10, 11]),
            ("b2", &vec![12, 13]),
            ("c2", &vec![14, 15]),
        );

        let err = join_collect(left, right, task_ctx).await.unwrap_err();

        assert_contains!(
            err.to_string(),
            "External error: Resources exhausted: Failed to allocate additional"
        );
        assert_contains!(err.to_string(), "CrossJoinExec");

        Ok(())
    }
}
