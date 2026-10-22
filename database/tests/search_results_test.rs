use anyhow::Result;
use sea_orm::{Database, DatabaseConnection};

use ::database::{
    actions::{
        collection::CollectionQueryType,
        search::{add_term, search_for},
    },
    connection::initialize_db,
};

const NODE_ID: &str = "search-test";

async fn library(tracks: &[(i32, &str)]) -> Result<DatabaseConnection> {
    let db = Database::connect("sqlite::memory:").await?;
    initialize_db(&db, NODE_ID).await?;
    for (id, name) in tracks {
        add_term(&db, CollectionQueryType::Track, *id, name).await?;
    }
    Ok(db)
}

async fn tracks_for(db: &DatabaseConnection, query: &str, n: usize) -> Result<Vec<i64>> {
    Ok(
        search_for(db, query, Some(vec![CollectionQueryType::Track]), n)
            .await?
            .remove(&CollectionQueryType::Track)
            .unwrap_or_default(),
    )
}

// A name is indexed as written and transliterated; an ASCII name is the same both ways.
#[tokio::test]
async fn each_match_is_found_once() -> Result<()> {
    let db = library(&[(1, "The Beatles"), (2, "Beatles for Sale")]).await?;
    let mut ids = tracks_for(&db, "Beatles", 30).await?;
    ids.sort();
    assert_eq!(ids, vec![1, 2]);
    Ok(())
}

#[tokio::test]
async fn at_most_n_matches_are_found() -> Result<()> {
    let db = library(&[(1, "Help"), (2, "Help!"), (3, "Help Me")]).await?;
    assert_eq!(tracks_for(&db, "Help", 2).await?.len(), 2);
    Ok(())
}
