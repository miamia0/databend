//  Copyright 2021 Datafuse Labs.
//
//  Licensed under the Apache License, Version 2.0 (the "License");
//  you may not use this file except in compliance with the License.
//  You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
//  Unless required by applicable law or agreed to in writing, software
//  distributed under the License is distributed on an "AS IS" BASIS,
//  WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
//  See the License for the specific language governing permissions and
//  limitations under the License.
//

use std::sync::Arc;

use common_base::tokio;
use common_datablocks::DataBlock;
use common_datavalues::prelude::Series;
use common_datavalues::prelude::SeriesFrom;
use common_datavalues::DataField;
use common_datavalues::DataSchemaRef;
use common_datavalues::DataSchemaRefExt;
use common_datavalues::DataType;
use common_exception::Result;
use common_meta_types::CreateTableReq;
use common_meta_types::TableMeta;
use common_planners::col;
use common_planners::lit;
use common_planners::Extras;
use databend_query::catalogs::Catalog;
use databend_query::sessions::QueryContext;
use databend_query::storages::fuse::io::Readers;
use databend_query::storages::fuse::meta::BlockMeta;
use databend_query::storages::fuse::meta::TableSnapshot;
use databend_query::storages::fuse::pruning::BlockPruner;
use databend_query::storages::fuse::TBL_OPT_KEY_CHUNK_BLOCK_NUM;
use databend_query::storages::fuse::TBL_OPT_KEY_SNAPSHOT_LOC;
use futures::TryStreamExt;

use crate::storages::fuse::table_test_fixture::TestFixture;

async fn apply_block_pruning(
    table_snapshot: &TableSnapshot,
    schema: DataSchemaRef,
    push_down: &Option<Extras>,
    ctx: Arc<QueryContext>,
) -> Result<Vec<BlockMeta>> {
    BlockPruner::new(table_snapshot)
        .apply(schema, push_down, ctx.as_ref())
        .await
}

#[tokio::test]
async fn test_block_pruner() -> Result<()> {
    let fixture = TestFixture::new().await;
    let ctx = fixture.ctx();

    let test_tbl_name = "test_index_helper";
    let test_schema = DataSchemaRefExt::create(vec![
        DataField::new("a", DataType::UInt64, false),
        DataField::new("b", DataType::UInt64, false),
    ]);

    // create test table
    let crate_table_plan = CreateTableReq {
        if_not_exists: false,
        db: fixture.default_db_name(),
        table: test_tbl_name.to_string(),
        table_meta: TableMeta {
            schema: test_schema.clone(),
            engine: "FUSE".to_string(),
            // make sure blocks will not be merged
            options: [(TBL_OPT_KEY_CHUNK_BLOCK_NUM.to_owned(), "1".to_owned())].into(),
            ..Default::default()
        },
    };

    let catalog = ctx.get_catalog();
    catalog.create_table(crate_table_plan).await?;

    // get table
    let table = catalog
        .get_table(fixture.default_db_name().as_str(), test_tbl_name)
        .await?;

    // prepare test blocks
    let num = 10;
    let blocks = (0..num)
        .into_iter()
        .map(|idx| {
            Ok(DataBlock::create_by_array(test_schema.clone(), vec![
                Series::new(vec![idx + 1, idx + 2, idx + 3]),
                Series::new(vec![idx * num + 1, idx * num + 2, idx * num + 3]),
            ]))
        })
        .collect::<Vec<_>>();

    let stream = Box::pin(futures::stream::iter(blocks));
    let r = table.append_data(ctx.clone(), stream).await?;
    table
        .commit_insertion(ctx.clone(), r.try_collect().await?, false)
        .await?;

    // get the latest tbl
    let table = catalog
        .get_table(fixture.default_db_name().as_str(), test_tbl_name)
        .await?;

    let snapshot_loc = table
        .get_table_info()
        .options()
        .get(TBL_OPT_KEY_SNAPSHOT_LOC)
        .unwrap();

    let reader = Readers::table_snapshot_reader(ctx.as_ref());
    let snapshot = reader.read(snapshot_loc.as_str()).await?;

    // no pruning
    let push_downs = None;
    let blocks = apply_block_pruning(
        &snapshot,
        table.get_table_info().schema(),
        &push_downs,
        ctx.clone(),
    )
    .await?;
    let rows: u64 = blocks.iter().map(|b| b.row_count).sum();
    assert_eq!(rows, num * 3u64);
    assert_eq!(10, blocks.len());

    // fully pruned
    let mut extra = Extras::default();
    let pred = col("a").gt(lit(30));
    extra.filters = vec![pred];

    let blocks = apply_block_pruning(
        &snapshot,
        table.get_table_info().schema(),
        &Some(extra),
        ctx.clone(),
    )
    .await?;
    assert_eq!(0, blocks.len());

    // one block pruned
    let mut extra = Extras::default();
    let pred = col("a").gt(lit(3)).and(col("b").gt(lit(3)));
    extra.filters = vec![pred];

    let blocks = apply_block_pruning(
        &snapshot,
        table.get_table_info().schema(),
        &Some(extra),
        ctx.clone(),
    )
    .await?;
    assert_eq!(num - 1, blocks.len() as u64);

    Ok(())
}
