use crate::{
    asset_transaction_file, confidential_transaction_file, construct_path, create_rng_from_seed,
    errors::Error, load_object, save_object, user_public_account_file, CTXInstruction, Instruction,
    COMMON_OBJECTS_DIR, MEDIATOR_PUBLIC_ACCOUNT_FILE, OFF_CHAIN_DIR, ON_CHAIN_DIR,
    SECRET_ACCOUNT_FILE,
};
use codec::{Decode, Encode};
use cryptography::{
    asset_id_from_ticker,
    asset_proofs::{CommitmentWitness, ElgamalSecretKey},
    mercat::{
        asset::AssetMediator, transaction::CtxMediator, AccountMemo, AssetTransactionMediator,
        AssetTxState, EncryptionKeys, FinalizedTransferTx, InitializedAssetTx, MediatorAccount,
        PubAccount, TransferTransactionMediator, TxState, TxSubstate,
    },
};
use curve25519_dalek::scalar::Scalar;
use lazy_static::lazy_static;
use log::info;
use metrics::timing;
use rand::{CryptoRng, RngCore};
use schnorrkel::{context::SigningContext, signing_context, ExpansionMode, MiniSecretKey};
use std::{path::PathBuf, time::Instant};

lazy_static! {
    static ref SIG_CTXT_ISSUE: SigningContext = signing_context(b"mercat/asset");
    static ref SIG_CTXT_TRANSACTION: SigningContext = signing_context(b"mercat/transaction");
}

fn generate_mediator_keys<R: RngCore + CryptoRng>(rng: &mut R) -> (AccountMemo, MediatorAccount) {
    let mediator_elg_secret_key = ElgamalSecretKey::new(Scalar::random(rng));
    let mediator_enc_key = EncryptionKeys {
        pblc: mediator_elg_secret_key.get_public_key().into(),
        scrt: mediator_elg_secret_key.into(),
    };

    let mediator_signing_pair =
        MiniSecretKey::generate_with(rng).expand_to_keypair(ExpansionMode::Ed25519);

    (
        // By default, the last processed tx counter is set to zero for account creation.
        AccountMemo::new(mediator_enc_key.pblc, mediator_signing_pair.public, 0),
        MediatorAccount {
            encryption_key: mediator_enc_key,
            signing_key: mediator_signing_pair,
        },
    )
}

pub fn process_create_mediator(seed: String, db_dir: PathBuf, user: String) -> Result<(), Error> {
    // Setup the rng.
    let mut rng = create_rng_from_seed(Some(seed))?;

    // Generate keys for the mediator.
    let mediator_key_gen_timer = Instant::now();
    let (public_account, private_account) = generate_mediator_keys(&mut rng);
    timing!("mediator.key_gen", mediator_key_gen_timer, Instant::now());

    let mediator_save_keys_timer = Instant::now();
    save_object(
        db_dir.clone(),
        ON_CHAIN_DIR,
        &user,
        MEDIATOR_PUBLIC_ACCOUNT_FILE,
        &public_account,
    )?;

    save_object(
        db_dir,
        OFF_CHAIN_DIR,
        &user,
        SECRET_ACCOUNT_FILE,
        &private_account,
    )?;
    timing!(
        "mediator.save_keys",
        mediator_save_keys_timer,
        Instant::now()
    );

    Ok(())
}

pub fn justify_asset_issuance(
    db_dir: PathBuf,
    issuer: String,
    mediator: String,
    ticker: String,
    tx_id: u32,
    reject: bool,
    cheat: bool,
) -> Result<(), Error> {
    // Load the transaction, mediator's credentials, and issuer's public account.
    let justify_load_objects_timer = Instant::now();

    let instruction_file_name = asset_transaction_file(
        tx_id,
        &issuer,
        AssetTxState::Initialization(TxSubstate::Started),
    );

    let instruction: Instruction = load_object(
        db_dir.clone(),
        ON_CHAIN_DIR,
        COMMON_OBJECTS_DIR,
        &instruction_file_name,
    )?;

    let asset_tx = InitializedAssetTx::decode(&mut &instruction.data[..]).map_err(|error| {
        Error::ObjectLoadError {
            error,
            path: construct_path(
                db_dir.clone(),
                ON_CHAIN_DIR,
                COMMON_OBJECTS_DIR,
                &instruction_file_name,
            ),
        }
    })?;

    let mediator_account: MediatorAccount = load_object(
        db_dir.clone(),
        OFF_CHAIN_DIR,
        &mediator,
        SECRET_ACCOUNT_FILE,
    )?;

    let issuer_account: PubAccount = load_object(
        db_dir.clone(),
        ON_CHAIN_DIR,
        &issuer,
        &user_public_account_file(&ticker),
    )?;

    timing!(
        "mediator.justify_load_objects",
        justify_load_objects_timer,
        Instant::now()
    );

    // Justification.
    let justify_library_timer = Instant::now();
    let mut justified_tx = AssetMediator {}
        .justify_asset_transaction(
            asset_tx.clone(),
            &issuer_account,
            &mediator_account.encryption_key,
            &mediator_account.signing_key,
        )
        .map_err(|error| Error::LibraryError { error })?;

    if cheat {
        info!(
            "CLI log: tx-{}: Cheating by overwriting the asset id of the account.",
            tx_id
        );
        let cheat_asset_id =
            asset_id_from_ticker("CHEAT").map_err(|error| Error::LibraryError { error })?;
        let cheat_asset_id_witness =
            CommitmentWitness::new(cheat_asset_id.clone().into(), Scalar::one());
        let cheat_enc_asset_id = mediator_account
            .clone()
            .encryption_key
            .pblc
            .encrypt(&cheat_asset_id_witness);

        justified_tx.content.content.enc_asset_id = cheat_enc_asset_id;
        let message = justified_tx.content.encode();
        justified_tx.sig = mediator_account
            .clone()
            .signing_key
            .sign(SIG_CTXT_ISSUE.bytes(&message));
    }

    timing!(
        "mediator.justify_library",
        justify_library_timer,
        Instant::now()
    );

    let next_instruction;
    let justify_save_objects_timer = Instant::now();
    // If the `reject` flag is set, save the transaction as rejected.
    if reject {
        info!(
            "CLI log: tx-{}: Rejecting the transaction as instructed.",
            tx_id
        );
        let rejected_state = AssetTxState::Justification(TxSubstate::Rejected);
        next_instruction = Instruction {
            data: asset_tx.encode().to_vec(),
            state: rejected_state,
        };

        save_object(
            db_dir,
            ON_CHAIN_DIR,
            COMMON_OBJECTS_DIR,
            &asset_transaction_file(tx_id, &issuer, rejected_state),
            &next_instruction,
        )?;
    } else {
        // Save the updated_issuer_account, and the justified transaction.
        next_instruction = Instruction {
            data: justified_tx.encode().to_vec(),
            state: AssetTxState::Justification(TxSubstate::Started),
        };

        save_object(
            db_dir,
            ON_CHAIN_DIR,
            COMMON_OBJECTS_DIR,
            &asset_transaction_file(
                tx_id,
                &mediator,
                AssetTxState::Justification(TxSubstate::Started),
            ),
            &next_instruction,
        )?;
    }

    timing!(
        "mediator.justify_save_objects",
        justify_save_objects_timer,
        Instant::now()
    );

    Ok(())
}

pub fn justify_asset_transaction(
    db_dir: PathBuf,
    sender: String,
    receiver: String,
    mediator: String,
    ticker: String,
    tx_id: u32,
    reject: bool,
    cheat: bool,
) -> Result<(), Error> {
    // Load the transaction, mediator's credentials, and issuer's public account.
    let justify_load_objects_timer = Instant::now();

    let instruction_path =
        confidential_transaction_file(tx_id, &sender, TxState::Finalization(TxSubstate::Started));
    let instruction: CTXInstruction = load_object(
        db_dir.clone(),
        ON_CHAIN_DIR,
        COMMON_OBJECTS_DIR,
        &instruction_path,
    )?;

    let asset_tx = FinalizedTransferTx::decode(&mut &instruction.data[..]).map_err(|error| {
        Error::ObjectLoadError {
            error,
            path: construct_path(
                db_dir.clone(),
                ON_CHAIN_DIR,
                COMMON_OBJECTS_DIR,
                &instruction_path,
            ),
        }
    })?;

    let mediator_account: MediatorAccount = load_object(
        db_dir.clone(),
        OFF_CHAIN_DIR,
        &mediator,
        SECRET_ACCOUNT_FILE,
    )?;

    let sender_account: PubAccount = load_object(
        db_dir.clone(),
        ON_CHAIN_DIR,
        &sender.clone(),
        &user_public_account_file(&ticker),
    )?;

    let receiver_account: PubAccount = load_object(
        db_dir.clone(),
        ON_CHAIN_DIR,
        &receiver.clone(),
        &user_public_account_file(&ticker),
    )?;

    timing!(
        "mediator.justify_tx.load_objects",
        justify_load_objects_timer,
        Instant::now()
    );

    // Justification.
    let justify_library_timer = Instant::now();
    let asset_id = asset_id_from_ticker(&ticker).map_err(|error| Error::LibraryError { error })?;
    let mut justified_tx = CtxMediator {}
        .justify_transaction(
            asset_tx.clone(),
            &mediator_account.encryption_key,
            &mediator_account.signing_key,
            &sender_account.memo.owner_sign_pub_key,
            &receiver_account.memo.owner_sign_pub_key,
            asset_id,
        )
        .map_err(|error| Error::LibraryError { error })?;

    if cheat {
        info!(
            "CLI log: tx-{}: Cheating by overwriting the sender's account id.",
            tx_id
        );

        justified_tx
            .content
            .content
            .init_data
            .content
            .memo
            .sndr_account_id += 1;
        let message = justified_tx.content.encode();
        justified_tx.sig = mediator_account
            .clone()
            .signing_key
            .sign(SIG_CTXT_TRANSACTION.bytes(&message));
    }

    timing!(
        "mediator.justify_tx.library",
        justify_library_timer,
        Instant::now()
    );

    let next_instruction;
    let justify_save_objects_timer = Instant::now();
    // If the `reject` flag is set, save the transaction as rejected.
    if reject {
        let rejected_state = TxState::Justification(TxSubstate::Rejected);
        next_instruction = CTXInstruction {
            data: asset_tx.encode().to_vec(),
            state: rejected_state,
        };

        save_object(
            db_dir.clone(),
            ON_CHAIN_DIR,
            COMMON_OBJECTS_DIR,
            &confidential_transaction_file(tx_id, &sender, rejected_state),
            &next_instruction,
        )?;
    } else {
        let new_state = TxState::Justification(TxSubstate::Started);
        // Save the updated_issuer_account, and the justified transaction.
        next_instruction = CTXInstruction {
            data: justified_tx.encode().to_vec(),
            state: new_state,
        };

        save_object(
            db_dir,
            ON_CHAIN_DIR,
            COMMON_OBJECTS_DIR,
            &confidential_transaction_file(tx_id, &mediator, new_state),
            &next_instruction,
        )?;
    }

    timing!(
        "mediator.justify_tx.save_objects",
        justify_save_objects_timer,
        Instant::now()
    );

    Ok(())
}
