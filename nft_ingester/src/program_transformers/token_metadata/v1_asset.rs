use crate::{program_transformers::common::task::DownloadMetadata, IngesterError};
use blockbuster::token_metadata::{
    pda::find_master_edition_account,
    state::{Metadata, TokenStandard, UseMethod, Uses},
};
use digital_asset_types::{
    dao::{
        asset, asset_authority, asset_creators, asset_data, asset_grouping,
        asset_v1_account_attachments,
        prelude::TokenAccounts,
        sea_orm_active_enums::{
            ChainMutability, Mutability, OwnerType, RoyaltyTargetType, SpecificationAssetClass,
            SpecificationVersions, V1AccountAttachments,
        },
        token_accounts, tokens,
    },
    json::ChainDataV1,
};
use num_traits::FromPrimitive;
use plerkle_serialization::Pubkey as FBPubkey;
use sea_orm::{
    entity::*,
    query::*,
    sea_query::OnConflict,
    ActiveValue::{Set},
    ConnectionTrait, DatabaseTransaction, DbBackend, DbErr, EntityTrait, JsonValue,
};


pub async fn save_v1_asset(
    id: FBPubkey,
    slot: u64,
    metadata: &Metadata,
    txn: &DatabaseTransaction,
) -> Result<DownloadMetadata, IngesterError> {
    let metadata = metadata.clone();
    let data = metadata.data;
    let meta_mint_pubkey = metadata.mint;
    let (edition_attachment_address, _) = find_master_edition_account(&meta_mint_pubkey);
    let mint = metadata.mint.to_bytes().to_vec();
    let authority = metadata.update_authority.to_bytes().to_vec();
    let id = id.0;
    let slot_i = slot as i64;
    let uri = data.uri.clone();
    let _spec = SpecificationVersions::V1;
    let class = match metadata.token_standard {
        Some(TokenStandard::NonFungible) => SpecificationAssetClass::Nft,
        Some(TokenStandard::FungibleAsset) => SpecificationAssetClass::FungibleAsset,
        Some(TokenStandard::Fungible) => SpecificationAssetClass::FungibleToken,
        _ => SpecificationAssetClass::Unknown,
    };
    let ownership_type = match class {
        SpecificationAssetClass::FungibleAsset => OwnerType::Token,
        SpecificationAssetClass::FungibleToken => OwnerType::Token,
        _ => OwnerType::Single,
    };

    let token_result: Option<(tokens::Model, Option<token_accounts::Model>)> = match ownership_type
    {
        OwnerType::Single => {
            let result = tokens::Entity::find_by_id(mint.clone())
                .find_also_related(TokenAccounts)
                .one(txn)
                .await?;
            Ok(result)
        }
        _ => {
            let token = tokens::Entity::find_by_id(mint.clone()).one(txn).await?;
            Ok(token.map(|t| (t, None)))
        }
    }
    .map_err(|e: DbErr| IngesterError::DatabaseError(e.to_string()))?;

    let (supply, supply_mint) = match token_result.clone() {
        Some((token, token_account)) => {
            let supply = match token_account {
                Some(ta) => ta.amount,
                None => token.supply,
            };
            (Set(supply), Set(Some(mint)))
        }
        None => (Set(1), NotSet),
    };

    let (owner, delegate) = match token_result {
        Some((_token, token_account)) => match token_account {
            Some(account) => (Set(Some(account.owner)), Set(account.delegate)),
            None => (NotSet, NotSet),
        },
        None => (NotSet, NotSet),
    };

    let mut chain_data = ChainDataV1 {
        name: data.name,
        symbol: data.symbol,
        edition_nonce: metadata.edition_nonce,
        primary_sale_happened: metadata.primary_sale_happened,
        token_standard: metadata.token_standard,
        uses: metadata.uses.map(|u| Uses {
            use_method: UseMethod::from_u8(u.use_method as u8).unwrap(),
            remaining: u.remaining,
            total: u.total,
        }),
    };
    chain_data.sanitize();
    let chain_data_json = serde_json::to_value(chain_data)
        .map_err(|e| IngesterError::DeserializationError(e.to_string()))?;
    let chain_mutability = match metadata.is_mutable {
        true => ChainMutability::Mutable,
        false => ChainMutability::Immutable,
    };

    let asset_data_model = asset_data::ActiveModel {
        chain_data_mutability: Set(chain_mutability),
        chain_data: Set(chain_data_json),
        metadata_url: Set(data.uri.trim().replace('\0', "")),
        metadata: Set(JsonValue::String("processing".to_string())),
        metadata_mutability: Set(Mutability::Mutable),
        slot_updated: Set(slot_i),
        id: Set(id.to_vec())
    };
    let query = asset_data::Entity::insert(asset_data_model)
        .on_conflict(
            OnConflict::columns([asset_data::Column::Id])
                .update_columns([
                    asset_data::Column::ChainDataMutability,
                    asset_data::Column::ChainData,
                    asset_data::Column::MetadataUrl,
                    asset_data::Column::Metadata,
                    asset_data::Column::MetadataMutability,
                    asset_data::Column::SlotUpdated,
                ])
                .to_owned(),
        )
        .build(DbBackend::Postgres);
    let res = txn.execute(query).await?;

    // Insert into `asset` table.
    let model = asset::ActiveModel {
        id: Set(id.to_vec()),
        owner,
        owner_type: Set(ownership_type),
        delegate,
        frozen: Set(false),
        supply,
        supply_mint,
        specification_version: Set(SpecificationVersions::V1),
        specification_asset_class: Set(class),
        tree_id: Set(None),
        nonce: Set(0),
        seq: Set(0),
        leaf: Set(None),
        compressed: Set(false),
        compressible: Set(false),
        royalty_target_type: Set(RoyaltyTargetType::Creators),
        royalty_target: Set(None),
        royalty_amount: Set(data.seller_fee_basis_points as i32), //basis points
        asset_data: Set(Some(id.to_vec())),
        slot_updated: Set(slot_i),
        burnt: Set(false),
        ..Default::default()
    };

    let query = asset::Entity::insert(model)
        .on_conflict(
            OnConflict::columns([asset::Column::Id])
                .update_columns([
                    asset::Column::Owner,
                    asset::Column::OwnerType,
                    asset::Column::Delegate,
                    asset::Column::Frozen,
                    asset::Column::Supply,
                    asset::Column::SupplyMint,
                    asset::Column::SpecificationVersion,
                    asset::Column::SpecificationAssetClass,
                    asset::Column::TreeId,
                    asset::Column::Nonce,
                    asset::Column::Seq,
                    asset::Column::Leaf,
                    asset::Column::Compressed,
                    asset::Column::Compressible,
                    asset::Column::RoyaltyTargetType,
                    asset::Column::RoyaltyTarget,
                    asset::Column::RoyaltyAmount,
                    asset::Column::AssetData,
                    asset::Column::SlotUpdated,
                    asset::Column::Burnt,
                ])
                .to_owned(),
        )
        .build(DbBackend::Postgres);
    txn.execute(query).await?;

    let attachment = asset_v1_account_attachments::ActiveModel {
        id: Set(edition_attachment_address.to_bytes().to_vec()),
        slot_updated: Set(slot_i),
        attachment_type: Set(V1AccountAttachments::MasterEditionV2),
        ..Default::default()
    };

    let query = asset_v1_account_attachments::Entity::insert(attachment)
        .on_conflict(
            OnConflict::columns([asset_v1_account_attachments::Column::Id])
                .do_nothing()
                .to_owned(),
        )
        .build(DbBackend::Postgres);
    txn.execute(query).await?;
    // Insert into `asset_creators` table.
    let creators = data.creators.unwrap_or_default();
    if !creators.is_empty() {
        let mut db_creators = Vec::with_capacity(creators.len());
        for c in creators.into_iter() {
            db_creators.push(asset_creators::ActiveModel {
                asset_id: Set(id.to_vec()),
                creator: Set(c.address.to_bytes().to_vec()),
                share: Set(c.share as i32),
                verified: Set(c.verified),
                seq: Set(0), // do we need this here @micheal-danenberg?
                slot_updated: Set(slot_i),
                ..Default::default()
            });
        }

        // Do not attempt to modify any existing values:
        // `ON CONFLICT ('asset_id') DO NOTHING`.
        let query = asset_creators::Entity::insert_many(db_creators)
            .on_conflict(
                OnConflict::columns([asset_creators::Column::AssetId])
                    .update_columns([
                        asset_creators::Column::Creator,
                        asset_creators::Column::Share,
                        asset_creators::Column::Verified,
                        asset_creators::Column::Seq,
                        asset_creators::Column::SlotUpdated,
                    ])
                    .to_owned(),
            )
            .build(DbBackend::Postgres);
        txn.execute(query).await?;

        // Insert into `asset_authority` table.
        let model = asset_authority::ActiveModel {
            asset_id: Set(id.to_vec()),
            authority: Set(authority),
            seq: Set(0),
            slot_updated: Set(slot_i),
            ..Default::default()
        };

        // Do not attempt to modify any existing values:
        // `ON CONFLICT ('asset_id') DO NOTHING`.
        let query = asset_authority::Entity::insert(model)
            .on_conflict(
                OnConflict::columns([asset_authority::Column::AssetId])
                    .update_columns([
                        asset_authority::Column::Authority,
                        asset_authority::Column::Seq,
                        asset_authority::Column::SlotUpdated,
                    ])
                    .to_owned(),
            )
            .build(DbBackend::Postgres);
        txn.execute(query).await?;

        // Insert into `asset_grouping` table.
        if let Some(c) = &metadata.collection {
            if c.verified {
                let model = asset_grouping::ActiveModel {
                    asset_id: Set(id.to_vec()),
                    group_key: Set("collection".to_string()),
                    group_value: Set(c.key.to_string()),
                    seq: Set(0),
                    slot_updated: Set(slot_i),
                    ..Default::default()
                };

                // Do not attempt to modify any existing values:
                // `ON CONFLICT ('asset_id') DO NOTHING`.
                let query = asset_grouping::Entity::insert(model)
                    .on_conflict(
                        OnConflict::columns([asset_grouping::Column::AssetId])
                            .update_columns([
                                asset_grouping::Column::GroupKey,
                                asset_grouping::Column::GroupValue,
                                asset_grouping::Column::Seq,
                                asset_grouping::Column::SlotUpdated,
                            ])
                            .to_owned(),
                    )
                    .build(DbBackend::Postgres);
                txn.execute(query).await?;
            }
        }
    }
    Ok(DownloadMetadata {
        asset_data_id: id.to_vec(),
        uri,
    })
}
