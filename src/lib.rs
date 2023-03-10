use near_contract_standards::fungible_token::core::ext_ft_core;
use near_contract_standards::fungible_token::receiver::FungibleTokenReceiver;
use near_sdk::borsh::{self, BorshDeserialize, BorshSerialize};
use near_sdk::collections::{LookupMap, LookupSet, Vector};
use near_sdk::json_types::{U128, U64};
use near_sdk::serde::{Deserialize, Serialize};
use near_sdk::{
    assert_one_yocto, env, near_bindgen, AccountId, Balance, BorshStorageKey,
    CryptoHash, Gas, PanicOnDefault, PromiseOrValue, PromiseResult, PublicKey,
};

const ECRECOVER_V: u8 = 0;
const ECRECOVER_M: bool = false;

const FEE_DENOMINATOR: u16 = 10000;

const GAS_FOR_RESOLVE_FULFILLED_SIG: Gas = Gas(30_000_000_000_000);

// //initiate a cross contract call to the nft contract. This will transfer the token to the buyer and return
// //a payout object used for the market to distribute funds to the appropriate accounts.
// #[ext_contract(ext_ft_contract)]
// trait ExtFtContract {
//     fn ft_transfer(
//         &mut self,
//         receiver_id: AccountId,
//         amount: U128,
//         memo: Option<String>
//     );
// }

#[derive(BorshDeserialize, BorshSerialize, Deserialize, Serialize)]
#[serde(crate = "near_sdk::serde")]
pub struct Transaction {
    pub from: String,
    pub to: String,
    pub amount: Balance,
    pub timestamp: U64,
    pub nonce: U128,
}

#[near_bindgen]
#[derive(BorshDeserialize, BorshSerialize, PanicOnDefault)]
struct BridgeAssist {
    pub owner: AccountId,
    pub relayer_role: PublicKey,
    pub token: AccountId,
    pub fee_wallet: AccountId,
    pub limit_per_send: Balance,
    pub nonce: U128,
    pub fee_numerator: u16,
    pub transactions: LookupMap<String, Vector<Transaction>>,
    pub fulfilled: LookupSet<String>,
}

/// Helper structure for keys of the persistent collections
#[derive(BorshStorageKey, BorshSerialize)]
pub enum StorageKey {
    Transactions,
    TransactionsInner { account_id_hash: CryptoHash },
    Fulfilled,
}

/*
    Trait that will be used as the callback from the FT contract. When ft_transfer_call() is
    called, it will fire a cross contract call to BridgeAssist and this is the function
    that is invoked.
*/
#[near_bindgen]
impl FungibleTokenReceiver for BridgeAssist {
    // Sends tokens on another chain by user
    fn ft_on_transfer(
        &mut self,
        sender_id: AccountId,
        amount: U128,
        msg: String,
    ) -> PromiseOrValue<U128> {
        // Require only specified FT can be used
        let ft_contract_id = env::predecessor_account_id();
        assert_eq!(
            env::predecessor_account_id(),
            self.token,
            "Only supports fungible token contract: {}",
            self.token
        );

        // Require the signer isn't the predecessor. This is so that we're sure
        // this was called via a cross-contract call from FT
        let signer_id = env::signer_account_id();
        assert_ne!(
            ft_contract_id, signer_id,
            "Should only be called via cross-contract call"
        );

        // Require the owner ID is the signer
        assert_eq!(sender_id, signer_id, "owner_id should be signer_id");

        // Limits check
        let mut amount = Balance::from(amount);
        let mut amount_to_return = Balance::from('0');
        if amount > self.limit_per_send {
            amount_to_return = amount - self.limit_per_send;
            amount = self.limit_per_send;
        }

        // @TODO: CHECK POSSIBILITY NOT TO USE CLONE
        let tx_data = Transaction {
            from: sender_id.clone().to_string(),
            to: msg.clone(),
            amount: amount.clone(),
            timestamp: U64::from(env::block_timestamp()),
            nonce: self.nonce.clone(),
        };

        let mut tx_vector = self.transactions.get(&tx_data.from).unwrap_or_else(|| {
            Vector::new(StorageKey::TransactionsInner {
                account_id_hash: env::sha256_array(sender_id.as_bytes()),
            })
        });
        tx_vector.push(&tx_data);
        self.transactions.insert(&tx_data.from, &tx_vector);
        self.nonce = U128::from(u128::from(self.nonce) + 1); // TODO: WHY U128 u128 wtf

        let log = format!(
            "Sent {} tokens from {} to {} in direction near->evm",
            amount,
            sender_id,
            msg.clone()
        );
        env::log_str(&log);
        PromiseOrValue::Value(U128::from(amount_to_return))
    }
}

#[near_bindgen]
impl BridgeAssist {
    #[init]
    pub fn init(
        owner: AccountId,
        relayer_role: String,
        token: AccountId,
        fee_wallet: AccountId,
        limit_per_send: Balance,
        fee_numerator: u16,
    ) -> Self {
        assert!(fee_numerator < FEE_DENOMINATOR, "Fee is to high");
        Self {
            owner,
            relayer_role: relayer_role.parse().unwrap(),
            token,
            fee_wallet,
            limit_per_send,
            nonce: U128::from(0),
            fee_numerator,
            transactions: LookupMap::new(StorageKey::Transactions),
            fulfilled: LookupSet::new(StorageKey::Fulfilled),
        }
    }

    // Fulfills transaction from another chain
    pub fn fulfill(&mut self, transaction: Transaction, signature: String) {
        assert_one_yocto();
        let to_user = AccountId::try_from(transaction.to.clone()).unwrap();

        // Tx reply check
        let tx_hash_bytes = env::keccak256(&bincode::serialize(&transaction).unwrap());
        let tx_hash = String::from_utf8(tx_hash_bytes.clone()).unwrap();
        assert!(
            !self.fulfilled.contains(&tx_hash),
            "Tx has already been fulfilled"
        );

        // Signature checks
        let sig_recover = env::ecrecover(
            &tx_hash_bytes,
            signature.as_bytes(),
            ECRECOVER_V,
            ECRECOVER_M,
        )
        .unwrap();
        assert_eq!(sig_recover, self.relayer_role.as_bytes(), "Wrong signature");

        self.fulfilled.insert(&tx_hash);
        let mut tx_vector = self.transactions.get(&transaction.from).unwrap_or_else(|| {
            Vector::new(StorageKey::TransactionsInner {
                account_id_hash: env::sha256_array(transaction.from.as_bytes()),
            })
        });
        tx_vector.push(&transaction);
        self.transactions.insert(&transaction.from, &tx_vector);

        let current_fee = transaction.amount * self.fee_numerator as u128 / FEE_DENOMINATOR as u128;
        let dispense_amount = transaction.amount - current_fee;

        let log = format!(
            "Dispense {} tokens from {} to {} in direction evm->near",
            dispense_amount, transaction.from, to_user
        );
        env::log_str(&log);

        // Transfer FT to user
        ext_ft_core::ext(self.token.clone())
            .with_attached_deposit(1)
            .ft_transfer(
                to_user,
                U128::from(dispense_amount),
                Some("Dispensing from bridge".to_string()),
            )
            .then(
                Self::ext(env::current_account_id())
                    .with_static_gas(GAS_FOR_RESOLVE_FULFILLED_SIG)
                    .resolve_fulfill(U128::from(dispense_amount), &tx_hash),
            );

        // If tx hash in set, dispense to user was successful, then transfer FT to fee_wallet
        if current_fee != 0 as u128 && self.fulfilled.contains(&tx_hash) {
            ext_ft_core::ext(self.token.clone())
            .with_attached_deposit(1)
            .ft_transfer(
                self.fee_wallet.clone(),
                U128::from(current_fee),
                Some("Transferring fee".to_string()),
            );
        }
    }

    // Callback for fulfill
    #[private]
    pub fn resolve_fulfill(&mut self, amount: U128, tx_hash: &String) -> U128 {
        let amount: Balance = amount.into();

        let revert_amount = match env::promise_result(0) {
            PromiseResult::NotReady => env::abort(),
            // If the promise was successful, get the return value and cast it to a U128.
            PromiseResult::Successful(_) => 0,
            // If the promise wasn't successful, return the original amount.
            PromiseResult::Failed => amount,
        };

        // If promise is failed remove txhash from fulfilled set
        if revert_amount > 0 {
            self.fulfilled.remove(tx_hash);
        }

        U128(revert_amount)
    }
}
