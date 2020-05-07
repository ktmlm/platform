#![deny(warnings)]
use bulletproofs::r1cs::R1CSProof;
use curve25519_dalek::ristretto::{CompressedRistretto, RistrettoPoint};
use curve25519_dalek::scalar::Scalar;
use ledger::data_model::errors::PlatformError;
use ledger::data_model::AssetTypeCode;
use ledger::error_location;
use linear_map::LinearMap;
use serde::{Deserialize, Serialize};
use txn_cli::txn_lib::query_utxo_and_get_amount;
use zei::crypto::solvency::{prove_solvency, verify_solvency};
use zei::errors::ZeiError;
use zei::setup::PublicParams;
use zei::xfr::structs::{asset_type_to_scalar, XfrAmount};

/// Scalar values of the amount and type code of an asset or liability.
pub(crate) type AmountAndCodeScalar = (Scalar, Scalar);
/// Commitment to the amount and associated type code of an asset or liability.
pub(crate) type AmountAndCodeCommitment = (CompressedRistretto, CompressedRistretto);
/// Blinding values of the amount and type code of a hidden asset of liability.
pub(crate) type AmountAndCodeBlinds = (Scalar, Scalar);
/// Type code and associated conversion rate.
pub(crate) type CodeAndRate = (Scalar, Scalar);

/// Indicates whether the amount is of an asset or liability.
pub enum AmountType {
  Asset,
  Liability,
}

pub(crate) fn get_decompressed_commitment(commitment: CompressedRistretto)
                                          -> Result<RistrettoPoint, PlatformError> {
  match commitment.decompress() {
    Some(decompressed_commitment) => Ok(decompressed_commitment),
    _ => Err(PlatformError::ZeiError(error_location!(), ZeiError::DecompressElementError)),
  }
}

/// Asset and liability information, and associated solvency proof if exists
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct AssetAndLiabilityAccount {
  /// Public asset amounts and associated codes
  pub public_assets: Vec<AmountAndCodeScalar>,

  /// Hidden asset amounts and associated codes
  pub hidden_assets: Vec<AmountAndCodeScalar>,

  /// Blinding values of hidden asset amounts and associated codes
  pub hidden_assets_blinds: Vec<AmountAndCodeBlinds>,

  /// Commitments to hidden asset amounts and associated codes
  pub hidden_assets_commitments: Vec<AmountAndCodeCommitment>,

  /// Public liability amounts and associated codes
  pub public_liabilities: Vec<AmountAndCodeScalar>,

  /// Hidden liability amounts and associated codes
  pub hidden_liabilities: Vec<AmountAndCodeScalar>,

  /// Blinding values of hidden liability amounts and associated codes
  pub hidden_liabilities_blinds: Vec<AmountAndCodeBlinds>,

  /// Commitments to hidden liability amounts and associated codes
  pub hidden_liabilities_commitments: Vec<AmountAndCodeCommitment>,

  /// Serialized solvency proof, null iff any of the following:
  /// * Solvency hasn't been proved
  /// * Assets or liabilities have been updated
  pub proof: Option<Vec<u8>>,
}

impl AssetAndLiabilityAccount {
  /// Queries a UTXO SID to get the amount or amount blinds and updates the account.
  /// * If the amount is public, verifies it and updates the list of public assets or liabilities.
  /// * Otherwise, updates the list of hidden assets or liabilities, the list of blinds, and the list of commitments.
  ///   * To add the blinds as (amount_blind, code_blind):
  ///     * Calculate (amount_blind, code_blind) = (amount_blind_low + POW_2_32 * amount_blind_high, code_blind).
  ///   * To add the commitment:
  ///     * Get the (amount_commitment_low, amount_commitment_high) from XfrAmount of the blind asset record.
  ///     * Calculate commitment = (amount_commitment_low + POW_2_32 * amount_commitment_high, code_commitment).
  #[allow(clippy::too_many_arguments)]
  pub fn update(&mut self,
                amount_type: AmountType,
                amount: u64,
                code: AssetTypeCode,
                blinds: Option<((Scalar, Scalar), Scalar)>,
                utxo: u64,
                protocol: &str,
                host: &str)
                -> Result<(), PlatformError> {
    let code_scalar = asset_type_to_scalar(&code.val);
    match query_utxo_and_get_amount(utxo, protocol, host)? {
      XfrAmount::NonConfidential(fetched_amount) => {
        if fetched_amount != amount {
          println!("Incorrect amount.");
          return Err(PlatformError::InputsError(error_location!()));
        }
        match amount_type {
          AmountType::Asset => {
            self.public_assets.push((Scalar::from(amount), code_scalar));
          }
          _ => {
            self.public_liabilities
                .push((Scalar::from(amount), code_scalar));
          }
        }
      }
      XfrAmount::Confidential((amount_commitment_low, amount_commitment_high)) => {
        let ((amount_blind_low, amount_blind_high), code_blind) = if let Some(b) = blinds {
          b
        } else {
          println!("Missing blinds for confidential amount.");
          return Err(PlatformError::InputsError(error_location!()));
        };
        let amount_and_code_blinds =
          (amount_blind_low + Scalar::from(1u64 << 32) * amount_blind_high, code_blind);
        let amount_commitment = (get_decompressed_commitment(amount_commitment_low)?
                                 + get_decompressed_commitment(amount_commitment_high)?
                                   * Scalar::from(1u64 << 32)).compress();
        let code_commitment = PublicParams::new().pc_gens
                                                 .commit(code_scalar, code_blind)
                                                 .compress();
        let commitment = (amount_commitment, code_commitment);
        match amount_type {
          AmountType::Asset => {
            self.hidden_assets
                .push((Scalar::from(amount), asset_type_to_scalar(&code.val)));
            self.hidden_assets_blinds.push(amount_and_code_blinds);
            self.hidden_assets_commitments.push(commitment);
          }
          _ => {
            self.hidden_liabilities
                .push((Scalar::from(amount), asset_type_to_scalar(&code.val)));
            self.hidden_liabilities_blinds.push(amount_and_code_blinds);
            self.hidden_liabilities_commitments.push(commitment);
          }
        }
      }
    }

    self.proof = None;
    Ok(())
  }
}

pub fn get_amount_and_code_blinds(blinds: Vec<((Scalar, Scalar), Scalar)>)
                                  -> Vec<(Scalar, Scalar)> {
  let mut amount_and_code_blinds = Vec::new();
  for ((amount_blind_low, amount_blind_high), code_blind) in blinds {
    amount_and_code_blinds.push((amount_blind_low + Scalar::from(1u64 << 32) * amount_blind_high,
                                 code_blind));
  }
  amount_and_code_blinds
}

/// Used to audit the solvency.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SolvencyAudit {
  /// Table mapping each asset code to its conversion rate.
  pub conversion_rates: Vec<CodeAndRate>,
}

impl SolvencyAudit {
  /// Sets conversion rate for the asset.
  pub fn set_rate(&mut self, code: AssetTypeCode, rate: u64) {
    self.conversion_rates
        .push((asset_type_to_scalar(&code.val), Scalar::from(rate)));
  }

  /// Proves the solvency and stores the commitments and proof.
  /// Must be used before `verify_solvency`.
  pub fn prove_solvency_and_store(&self,
                                  account: &mut AssetAndLiabilityAccount)
                                  -> Result<(), PlatformError> {
    // Prove the solvency
    let mut rates = LinearMap::new();
    for (code, rate) in self.conversion_rates.clone() {
      rates.insert(code, rate);
    }

    let proof =
      prove_solvency(&account.hidden_assets,
                     &account.hidden_assets_blinds,
                     &account.public_assets,
                     &account.hidden_liabilities,
                     &account.hidden_liabilities_blinds,
                     &account.public_liabilities,
                     &rates).or_else(|e| Err(PlatformError::ZeiError(error_location!(), e)))?;

    // Update data
    account.proof = Some(proof.to_bytes());
    Ok(())
  }

  /// Verifies the solvency proof.
  /// Must not be used before `prove_solvency_and_store`.
  pub fn verify_solvency(&self, account: &AssetAndLiabilityAccount) -> Result<(), PlatformError> {
    let proof = if let Some(p) = &account.proof {
      R1CSProof::from_bytes(p).or(Err(PlatformError::DeserializationError))?
    } else {
      println!("Prove the solvency first.");
      return Err(PlatformError::InputsError(error_location!()));
    };
    let mut rates = LinearMap::new();
    for (code, rate) in self.conversion_rates.clone() {
      rates.insert(code, rate);
    }
    verify_solvency(&account.hidden_assets_commitments,
                    &account.public_assets,
                    &account.hidden_liabilities_commitments,
                    &account.public_liabilities,
                    &rates,
                    &proof).or_else(|e| Err(PlatformError::ZeiError(error_location!(), e)))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use ledger::data_model::{AssetRules, AssetTypeCode};
  use ledger_standalone::LedgerStandalone;
  use rand_chacha::ChaChaRng;
  use rand_core::{CryptoRng, RngCore, SeedableRng};
  use txn_cli::txn_lib::{define_and_submit, issue_transfer_and_get_utxo_and_blinds};
  use zei::xfr::asset_record::AssetRecordType;
  use zei::xfr::sig::XfrKeyPair;

  const PROTOCOL: &str = "http";
  const HOST: &str = "localhost";

  // Randomly generate a key pair and three asset codes
  fn generate_key_pair_and_define_assets(ledger_standalone: &LedgerStandalone)
                                         -> (XfrKeyPair, Vec<AssetTypeCode>) {
    let codes = vec![AssetTypeCode::gen_random(),
                     AssetTypeCode::gen_random(),
                     AssetTypeCode::gen_random()];
    let key_pair = XfrKeyPair::generate(&mut ChaChaRng::from_entropy());
    for code in codes.iter() {
      define_and_submit(&key_pair, *code, AssetRules::default(), ledger_standalone).unwrap();
    }
    (key_pair, codes)
  }

  // Add three public asset amounts
  fn add_public_asset_amounts<R: CryptoRng + RngCore>(issuer_key_pair: &XfrKeyPair,
                                                      recipient_key_pair: &XfrKeyPair,
                                                      account: &mut AssetAndLiabilityAccount,
                                                      codes: &Vec<AssetTypeCode>,
                                                      prng: &mut R,
                                                      ledger_standalone: &LedgerStandalone)
                                                      -> Result<(), PlatformError> {
    let (utxo_0, amount_blinds_0, code_blind_0) =
      issue_transfer_and_get_utxo_and_blinds(issuer_key_pair,
                                                  recipient_key_pair,
                                                  100,
                                                  codes[0],
                                                  AssetRecordType::NonConfidentialAmount_NonConfidentialAssetType,
                                                  0,prng,
                                                  ledger_standalone)?;
    let (utxo_1, amount_blinds_1, code_blind_1) =
      issue_transfer_and_get_utxo_and_blinds(issuer_key_pair,
                                                  recipient_key_pair,
                                                  200,
                                                  codes[1],
                                                  AssetRecordType::NonConfidentialAmount_NonConfidentialAssetType,
                                                  0,prng,
                                                  ledger_standalone)?;
    let (utxo_2, amount_blinds_2, code_blind_2) =
      issue_transfer_and_get_utxo_and_blinds(issuer_key_pair,
                                                  recipient_key_pair,
                                                  300,
                                                  codes[2],
                                                  AssetRecordType::NonConfidentialAmount_NonConfidentialAssetType,
                                                  0,prng,
                                                  ledger_standalone)?;

    account.update(AmountType::Asset,
                   100,
                   codes[0],
                   Some((amount_blinds_0, code_blind_0)),
                   utxo_0,
                   PROTOCOL,
                   HOST)?;
    account.update(AmountType::Asset,
                   200,
                   codes[1],
                   Some((amount_blinds_1, code_blind_1)),
                   utxo_1,
                   PROTOCOL,
                   HOST)?;
    account.update(AmountType::Asset,
                   300,
                   codes[2],
                   Some((amount_blinds_2, code_blind_2)),
                   utxo_2,
                   PROTOCOL,
                   HOST)?;

    Ok(())
  }

  // Add three hidden asset amounts
  fn add_hidden_asset_amounts<R: CryptoRng + RngCore>(issuer_key_pair: &XfrKeyPair,
                                                      recipient_key_pair: &XfrKeyPair,
                                                      account: &mut AssetAndLiabilityAccount,
                                                      codes: &Vec<AssetTypeCode>,
                                                      prng: &mut R,
                                                      ledger_standalone: &LedgerStandalone)
                                                      -> Result<(), PlatformError> {
    let (utxo_0, amount_blinds_0, code_blind_0) =
      issue_transfer_and_get_utxo_and_blinds(issuer_key_pair,
                                                  recipient_key_pair,
                                                  10,
                                                  codes[0],
                                                  AssetRecordType::ConfidentialAmount_NonConfidentialAssetType,
                                                  1,prng,
                                                  ledger_standalone)?;
    let (utxo_1, amount_blinds_1, code_blind_1) =
      issue_transfer_and_get_utxo_and_blinds(issuer_key_pair,
                                                  recipient_key_pair,
                                                  20,
                                                  codes[1],
                                                  AssetRecordType::ConfidentialAmount_NonConfidentialAssetType,
                                                  1,prng,
                                                  ledger_standalone)?;
    let (utxo_2, amount_blinds_2, code_blind_2) =
      issue_transfer_and_get_utxo_and_blinds(issuer_key_pair,
                                                  recipient_key_pair,
                                                  30,
                                                  codes[2],
                                                  AssetRecordType::ConfidentialAmount_NonConfidentialAssetType,
                                                  1,prng,
                                                  ledger_standalone)?;

    account.update(AmountType::Asset,
                   10,
                   codes[0],
                   Some((amount_blinds_0, code_blind_0)),
                   utxo_0,
                   PROTOCOL,
                   HOST)?;
    account.update(AmountType::Asset,
                   20,
                   codes[1],
                   Some((amount_blinds_1, code_blind_1)),
                   utxo_1,
                   PROTOCOL,
                   HOST)?;
    account.update(AmountType::Asset,
                   30,
                   codes[2],
                   Some((amount_blinds_2, code_blind_2)),
                   utxo_2,
                   PROTOCOL,
                   HOST)?;

    Ok(())
  }

  // Add three public liability amounts
  fn add_public_liability_amounts<R: CryptoRng + RngCore>(issuer_key_pair: &XfrKeyPair,
                                                          recipient_key_pair: &XfrKeyPair,
                                                          account: &mut AssetAndLiabilityAccount,
                                                          codes: &Vec<AssetTypeCode>,
                                                          prng: &mut R,
                                                          ledger_standalone: &LedgerStandalone)
                                                          -> Result<(), PlatformError> {
    let (utxo_0, amount_blinds_0, code_blind_0) =
      issue_transfer_and_get_utxo_and_blinds(issuer_key_pair,
                                                  recipient_key_pair,
                                                  100,
                                                  codes[0],
                                                  AssetRecordType::NonConfidentialAmount_NonConfidentialAssetType,
                                                  2,prng,
                                                  ledger_standalone)?;
    let (utxo_1, amount_blinds_1, code_blind_1)=
      issue_transfer_and_get_utxo_and_blinds(issuer_key_pair,
                                                  recipient_key_pair,
                                                  200,
                                                  codes[1],
                                                  AssetRecordType::NonConfidentialAmount_NonConfidentialAssetType,
                                                  2,prng,
                                                  ledger_standalone)?;
    let (utxo_2, amount_blinds_2, code_blind_2) =
      issue_transfer_and_get_utxo_and_blinds(issuer_key_pair,
                                                  recipient_key_pair,
                                                  200,
                                                  codes[2],
                                                  AssetRecordType::NonConfidentialAmount_NonConfidentialAssetType,
                                                  2,prng,
                                                  ledger_standalone)?;

    account.update(AmountType::Liability,
                   100,
                   codes[0],
                   Some((amount_blinds_0, code_blind_0)),
                   utxo_0,
                   PROTOCOL,
                   HOST)?;
    account.update(AmountType::Liability,
                   200,
                   codes[1],
                   Some((amount_blinds_1, code_blind_1)),
                   utxo_1,
                   PROTOCOL,
                   HOST)?;
    account.update(AmountType::Liability,
                   200,
                   codes[2],
                   Some((amount_blinds_2, code_blind_2)),
                   utxo_2,
                   PROTOCOL,
                   HOST)?;

    Ok(())
  }

  // Add three hidden liability amounts, with total value smaller than hidden assets'
  fn add_hidden_liability_amounts_smaller<R: CryptoRng + RngCore>(issuer_key_pair: &XfrKeyPair,
                                                                  recipient_key_pair: &XfrKeyPair,
                                                                  account: &mut AssetAndLiabilityAccount,
                                                                  codes: &Vec<AssetTypeCode>,
                                                                  prng: &mut R,
                                                                  ledger_standalone: &LedgerStandalone)
                                                                  -> Result<(), PlatformError> {
    let (utxo_0, amount_blinds_0, code_blind_0) =
      issue_transfer_and_get_utxo_and_blinds(issuer_key_pair,
                                                  recipient_key_pair,
                                                  10,
                                                  codes[0],
                                                  AssetRecordType::ConfidentialAmount_NonConfidentialAssetType,
                                                  3,prng,
                                                  ledger_standalone)?;
    let (utxo_1, amount_blinds_1, code_blind_1) =
      issue_transfer_and_get_utxo_and_blinds(issuer_key_pair,
                                                  recipient_key_pair,
                                                  20,
                                                  codes[1],
                                                  AssetRecordType::ConfidentialAmount_NonConfidentialAssetType,
                                                  3,prng,
                                                  ledger_standalone)?;
    let (utxo_2, amount_blinds_2, code_blind_2) =
      issue_transfer_and_get_utxo_and_blinds(issuer_key_pair,
                                                  recipient_key_pair,
                                                  20,
                                                  codes[2],
                                                  AssetRecordType::ConfidentialAmount_NonConfidentialAssetType,
                                                  3,prng,
                                                  ledger_standalone)?;

    account.update(AmountType::Liability,
                   10,
                   codes[0],
                   Some((amount_blinds_0, code_blind_0)),
                   utxo_0,
                   PROTOCOL,
                   HOST)?;
    account.update(AmountType::Liability,
                   20,
                   codes[1],
                   Some((amount_blinds_1, code_blind_1)),
                   utxo_1,
                   PROTOCOL,
                   HOST)?;
    account.update(AmountType::Liability,
                   20,
                   codes[2],
                   Some((amount_blinds_2, code_blind_2)),
                   utxo_2,
                   PROTOCOL,
                   HOST)?;

    Ok(())
  }

  // Add three hidden liability amounts, with total value larger than hidden assets'
  fn add_hidden_liability_amounts_larger<R: CryptoRng + RngCore>(issuer_key_pair: &XfrKeyPair,
                                                                 recipient_key_pair: &XfrKeyPair,
                                                                 account: &mut AssetAndLiabilityAccount,
                                                                 codes: &Vec<AssetTypeCode>,
                                                                 prng: &mut R,
                                                                 ledger_standalone: &LedgerStandalone)
                                                                 -> Result<(), PlatformError> {
    let (utxo_0, amount_blinds_0, code_blind_0) =
      issue_transfer_and_get_utxo_and_blinds(issuer_key_pair,
                                                  recipient_key_pair,
                                                  10,
                                                  codes[0],
                                                  AssetRecordType::ConfidentialAmount_NonConfidentialAssetType,
                                                  4,prng,
                                                  ledger_standalone)?;
    let (utxo_1, amount_blinds_1, code_blind_1) =
      issue_transfer_and_get_utxo_and_blinds(issuer_key_pair,
                                                  recipient_key_pair,
                                                  20,
                                                  codes[1],
                                                  AssetRecordType::ConfidentialAmount_NonConfidentialAssetType,
                                                  4,prng,
                                                  ledger_standalone)?;
    let (utxo_2, amount_blinds_2, code_blind_2) =
      issue_transfer_and_get_utxo_and_blinds(issuer_key_pair,
                                                  recipient_key_pair,
                                                  40,
                                                  codes[2],
                                                  AssetRecordType::ConfidentialAmount_NonConfidentialAssetType,
                                                  4,prng,
                                                  ledger_standalone)?;

    account.update(AmountType::Liability,
                   10,
                   codes[0],
                   Some((amount_blinds_0, code_blind_0)),
                   utxo_0,
                   PROTOCOL,
                   HOST)?;
    account.update(AmountType::Liability,
                   20,
                   codes[1],
                   Some((amount_blinds_1, code_blind_1)),
                   utxo_1,
                   PROTOCOL,
                   HOST)?;
    account.update(AmountType::Liability,
                   40,
                   codes[2],
                   Some((amount_blinds_2, code_blind_2)),
                   utxo_2,
                   PROTOCOL,
                   HOST)?;

    Ok(())
  }

  // Add asset conversion rates
  fn add_conversion_rates(audit: &mut SolvencyAudit, codes: Vec<AssetTypeCode>) {
    let mut rate = 1;
    for code in codes.iter() {
      audit.set_rate(*code, rate);
      rate += 1;
    }
  }

  #[test]
  #[ignore]
  fn test_prove_solvency_fail() {
    // Start the standalone ledger
    let ledger_standalone = &LedgerStandalone::new();
    ledger_standalone.poll_until_ready().unwrap();

    // Start a solvency audit process
    let mut audit = SolvencyAudit::default();

    // Generate issuer key pair and define assets
    let (issuer_key_pair, codes) = generate_key_pair_and_define_assets(ledger_standalone);

    // Set asset conversion rates, but miss one asset
    add_conversion_rates(&mut audit, vec![codes[0].clone(), codes[1].clone()]);

    // Create an asset and liability account
    let mut account = AssetAndLiabilityAccount::default();
    let recipient_key_pair = &XfrKeyPair::generate(&mut ChaChaRng::from_entropy());
    let prng = &mut ChaChaRng::from_entropy();
    add_hidden_asset_amounts(&issuer_key_pair,
                             recipient_key_pair,
                             &mut account,
                             &codes,
                             prng,
                             ledger_standalone).unwrap();
    add_hidden_liability_amounts_smaller(&issuer_key_pair,
                                         recipient_key_pair,
                                         &mut account,
                                         &codes,
                                         prng,
                                         ledger_standalone).unwrap();

    // Prove the solvency
    // Should fail with ZeiError::SolvencyProveError
    match audit.prove_solvency_and_store(&mut account) {
      Err(PlatformError::ZeiError(_, ZeiError::SolvencyProveError)) => {}
      unexpected_result => {
        panic!(format!("Expected ZeiError::SolvencyProveError, found {:?}.",
                       unexpected_result));
      }
    }
  }

  #[test]
  #[ignore]
  fn test_verify_solvency_fail() {
    // Start the standalone ledger
    let ledger_standalone = &LedgerStandalone::new();
    ledger_standalone.poll_until_ready().unwrap();

    // Start a solvency audit process
    let mut audit = SolvencyAudit::default();

    // Generate issuer key pair and define assets
    let (issuer_key_pair, codes) = generate_key_pair_and_define_assets(ledger_standalone);

    // Set asset conversion rates
    add_conversion_rates(&mut audit, codes.clone());

    // Create an asset and liability account
    let mut account = AssetAndLiabilityAccount::default();
    let recipient_key_pair = &XfrKeyPair::generate(&mut ChaChaRng::from_entropy());
    let prng = &mut ChaChaRng::from_entropy();
    add_hidden_asset_amounts(&issuer_key_pair,
                             recipient_key_pair,
                             &mut account,
                             &codes,
                             prng,
                             ledger_standalone).unwrap();
    add_hidden_liability_amounts_smaller(&issuer_key_pair,
                                         recipient_key_pair,
                                         &mut account,
                                         &codes,
                                         prng,
                                         ledger_standalone).unwrap();

    // Verify the solvency without a proof
    // Should fail with InputsError
    match audit.verify_solvency(&account) {
      Err(PlatformError::InputsError(_)) => {}
      unexpected_result => {
        panic!(format!("Expected InputsError, found {:?}.", unexpected_result));
      }
    }
  }

  #[test]
  #[ignore]
  fn test_prove_and_verify_solvency_fail() {
    // Start the standalone ledger
    let ledger_standalone = &LedgerStandalone::new();
    ledger_standalone.poll_until_ready().unwrap();

    // Start a solvency audit process
    let mut audit = SolvencyAudit::default();

    // Generate issuer key pair and define assets
    let (issuer_key_pair, codes) = generate_key_pair_and_define_assets(ledger_standalone);

    // Set asset conversion rates
    add_conversion_rates(&mut audit, codes.clone());

    // Create an asset and liability account
    let mut account = AssetAndLiabilityAccount::default();

    // Adds hidden assets
    let recipient_key_pair = &XfrKeyPair::generate(&mut ChaChaRng::from_entropy());
    let prng = &mut ChaChaRng::from_entropy();
    add_hidden_asset_amounts(&issuer_key_pair,
                             recipient_key_pair,
                             &mut account,
                             &codes,
                             prng,
                             ledger_standalone).unwrap();

    // Adds hidden liabilities, with total value larger than hidden assets'
    add_hidden_liability_amounts_larger(&issuer_key_pair,
                                        recipient_key_pair,
                                        &mut account,
                                        &codes,
                                        prng,
                                        ledger_standalone).unwrap();

    // Prove the solvency
    audit.prove_solvency_and_store(&mut account).unwrap();
    assert!(account.proof.is_some());

    // Verify the solvency proof
    // Should fail with ZeiError::SolvencyVerificationError
    match audit.verify_solvency(&account) {
      Err(PlatformError::ZeiError(_, ZeiError::SolvencyVerificationError)) => {}
      unexpected_result => {
        panic!(format!("Expected ZeiError::SolvencyVerificationError, found {:?}.",
                       unexpected_result));
      }
    }
  }

  #[test]
  #[ignore]
  fn test_prove_and_verify_solvency_simple_pass() {
    // Start the standalone ledger
    let ledger_standalone = &LedgerStandalone::new();
    ledger_standalone.poll_until_ready().unwrap();

    // Start a solvency audit process
    let mut audit = SolvencyAudit::default();

    // Generate issuer key pair and define assets
    let issuer_key_pair = &XfrKeyPair::generate(&mut ChaChaRng::from_entropy());
    let code = AssetTypeCode::gen_random();
    define_and_submit(issuer_key_pair,
                      code,
                      AssetRules::default(),
                      ledger_standalone).unwrap();

    // Set asset conversion rates
    audit.set_rate(code, 1);

    // Create an asset and liability account
    let mut account = AssetAndLiabilityAccount::default();
    let recipient_key_pair = &XfrKeyPair::generate(&mut ChaChaRng::from_entropy());
    let prng = &mut ChaChaRng::from_entropy();
    let (utxo, amount_blinds, code_blind) =
    issue_transfer_and_get_utxo_and_blinds(issuer_key_pair,
                                           recipient_key_pair,
                                           10,
                                           code,
                                           AssetRecordType::ConfidentialAmount_NonConfidentialAssetType,
                                           1,
                                           prng,
                                           ledger_standalone).unwrap();

    account.update(AmountType::Asset,
                   10,
                   code,
                   Some((amount_blinds, code_blind)),
                   utxo,
                   PROTOCOL,
                   HOST)
           .unwrap();

    // Prove the solvency
    audit.prove_solvency_and_store(&mut account).unwrap();
    assert!(account.proof.is_some());

    // Verify the solvency proof
    audit.verify_solvency(&account).unwrap();
  }

  #[test]
  #[ignore]
  fn test_prove_and_verify_solvency_complex_pass() {
    // Start the standalone ledger
    let ledger_standalone = &LedgerStandalone::new();
    ledger_standalone.poll_until_ready().unwrap();

    // Start a solvency audit process
    let mut audit = SolvencyAudit::default();

    // Generate issuer key pair and define assets
    let (issuer_key_pair, codes) = generate_key_pair_and_define_assets(ledger_standalone);

    // Set asset conversion rates
    add_conversion_rates(&mut audit, codes.clone());

    // Create an asset and liability account
    let mut account = AssetAndLiabilityAccount::default();
    let recipient_key_pair = &XfrKeyPair::generate(&mut ChaChaRng::from_entropy());
    let prng = &mut ChaChaRng::from_entropy();
    add_public_asset_amounts(&issuer_key_pair,
                             recipient_key_pair,
                             &mut account,
                             &codes,
                             prng,
                             ledger_standalone).unwrap();
    add_hidden_asset_amounts(&issuer_key_pair,
                             recipient_key_pair,
                             &mut account,
                             &codes,
                             prng,
                             ledger_standalone).unwrap();
    add_public_liability_amounts(&issuer_key_pair,
                                 recipient_key_pair,
                                 &mut account,
                                 &codes,
                                 prng,
                                 ledger_standalone).unwrap();
    add_hidden_liability_amounts_smaller(&issuer_key_pair,
                                         recipient_key_pair,
                                         &mut account,
                                         &codes,
                                         prng,
                                         ledger_standalone).unwrap();

    // Prove the solvency
    audit.prove_solvency_and_store(&mut account).unwrap();
    assert!(account.proof.is_some());

    // Verify the solvency proof
    audit.verify_solvency(&account).unwrap();
  }

  #[test]
  #[ignore]
  fn test_update_asset_and_verify_solvency_mixed() {
    // Start the standalone ledger
    let ledger_standalone = &LedgerStandalone::new();
    ledger_standalone.poll_until_ready().unwrap();

    // Start a solvency audit process
    let mut audit = SolvencyAudit::default();

    // Generate issuer key pair and define assets
    let (issuer_key_pair, codes) = generate_key_pair_and_define_assets(ledger_standalone);

    // Set asset conversion rates
    add_conversion_rates(&mut audit, codes.clone());

    // Create an asset and liability account
    let mut account = AssetAndLiabilityAccount::default();
    let recipient_key_pair = &XfrKeyPair::generate(&mut ChaChaRng::from_entropy());
    let prng = &mut ChaChaRng::from_entropy();
    add_public_asset_amounts(&issuer_key_pair,
                             recipient_key_pair,
                             &mut account,
                             &codes,
                             prng,
                             ledger_standalone).unwrap();
    add_hidden_asset_amounts(&issuer_key_pair,
                             recipient_key_pair,
                             &mut account,
                             &codes,
                             prng,
                             ledger_standalone).unwrap();
    add_public_liability_amounts(&issuer_key_pair,
                                 recipient_key_pair,
                                 &mut account,
                                 &codes,
                                 prng,
                                 ledger_standalone).unwrap();
    add_hidden_liability_amounts_smaller(&issuer_key_pair,
                                         recipient_key_pair,
                                         &mut account,
                                         &codes,
                                         prng,
                                         ledger_standalone).unwrap();

    // Prove and verify the solvency
    audit.prove_solvency_and_store(&mut account).unwrap();
    audit.verify_solvency(&account).unwrap();

    // Update the public assets
    let (utxo, amount_blinds, code_blind) = issue_transfer_and_get_utxo_and_blinds(&issuer_key_pair,
                                                              recipient_key_pair,
                                                              40,
                                                              codes[0],
                                                              AssetRecordType::NonConfidentialAmount_NonConfidentialAssetType,
                                                              5,
                                                              prng,
                                                              ledger_standalone).unwrap();
    account.update(AmountType::Liability,
                   40,
                   codes[0],
                   Some((amount_blinds, code_blind)),
                   utxo,
                   PROTOCOL,
                   HOST)
           .unwrap();

    // Verify the solvency without proving it again
    // Should fail with InputsError
    match audit.verify_solvency(&account) {
      Err(PlatformError::InputsError(_)) => {}
      unexpected_result => {
        panic!(format!("Expected InputsError, found {:?}.", unexpected_result));
      }
    }

    // Prove the solvency again and verify the proof
    audit.prove_solvency_and_store(&mut account).unwrap();
    audit.verify_solvency(&account).unwrap();
  }

  #[test]
  #[ignore]
  fn test_update_liability_and_verify_solvency_fail() {
    // Start the standalone ledger
    let ledger_standalone = &LedgerStandalone::new();
    ledger_standalone.poll_until_ready().unwrap();

    // Start a solvency audit process
    let mut audit = SolvencyAudit::default();

    // Generate issuer key pair and define assets
    let (issuer_key_pair, codes) = generate_key_pair_and_define_assets(ledger_standalone);

    // Set asset conversion rates
    add_conversion_rates(&mut audit, codes.clone());

    // Create an asset and liability account
    let mut account = AssetAndLiabilityAccount::default();
    let recipient_key_pair = &XfrKeyPair::generate(&mut ChaChaRng::from_entropy());
    let prng = &mut ChaChaRng::from_entropy();
    add_public_asset_amounts(&issuer_key_pair,
                             recipient_key_pair,
                             &mut account,
                             &codes,
                             prng,
                             ledger_standalone).unwrap();
    add_hidden_asset_amounts(&issuer_key_pair,
                             recipient_key_pair,
                             &mut account,
                             &codes,
                             prng,
                             ledger_standalone).unwrap();
    add_public_liability_amounts(&issuer_key_pair,
                                 recipient_key_pair,
                                 &mut account,
                                 &codes,
                                 prng,
                                 ledger_standalone).unwrap();
    add_hidden_liability_amounts_smaller(&issuer_key_pair,
                                         recipient_key_pair,
                                         &mut account,
                                         &codes,
                                         prng,
                                         ledger_standalone).unwrap();

    // Prove and verify the solvency
    audit.prove_solvency_and_store(&mut account).unwrap();
    audit.verify_solvency(&account).unwrap();

    // Update the hidden liabilities
    let (utxo, amount_blinds, code_blind) = issue_transfer_and_get_utxo_and_blinds(&issuer_key_pair,
                                                                     recipient_key_pair,
                                                                     4000,
                                                                     codes[0],
                                                                     AssetRecordType::ConfidentialAmount_NonConfidentialAssetType,
                                                                     5,prng,
                                                                     ledger_standalone).unwrap();
    account.update(AmountType::Liability,
                   4000,
                   codes[0],
                   Some((amount_blinds, code_blind)),
                   utxo,
                   PROTOCOL,
                   HOST)
           .unwrap();

    // Prove the solvency again
    audit.prove_solvency_and_store(&mut account).unwrap();

    // Verify the solvency proof
    // Should fail with SolvencyVerificationError
    match audit.verify_solvency(&account) {
      Err(PlatformError::ZeiError(_, ZeiError::SolvencyVerificationError)) => {}
      unexpected_result => {
        panic!(format!("Expected ZeiError::SolvencyVerificationError, found {:?}.",
                       unexpected_result));
      }
    }
  }
}