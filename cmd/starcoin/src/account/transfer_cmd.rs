// Copyright (c) The Starcoin Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::cli_state::CliState;
use crate::view::{ExecuteResultView, ExecutionOutputView};
use crate::StarcoinOpt;
use anyhow::{format_err, Result};
use scmd::{CommandAction, ExecContext};
use starcoin_crypto::{ed25519::Ed25519PublicKey, ValidCryptoMaterialStringExt};
use starcoin_executor::DEFAULT_EXPIRATION_TIME;
use starcoin_rpc_client::RemoteStateReader;
use starcoin_state_api::AccountStateReader;
use starcoin_types::receipt_identifier::ReceiptIdentifier;
use starcoin_vm_types::account_address::AccountAddress;
use starcoin_vm_types::token::stc::STC_TOKEN_CODE;
use starcoin_vm_types::token::token_code::TokenCode;
use starcoin_vm_types::transaction::authenticator::AuthenticationKey;
use structopt::StructOpt;

#[derive(Debug, StructOpt)]
#[structopt(name = "transfer")]
pub struct TransferOpt {
    #[structopt(short = "s")]
    /// if `sender` is absent, use default account.
    sender: Option<AccountAddress>,

    #[structopt(long = "receipt", name = "receipt")]
    /// receipt identifier
    receipt_identifier: Option<ReceiptIdentifier>,

    #[structopt(short = "r", required_unless = "receipt")]
    /// if receipt is absent, receiver must be provided.
    receiver: Option<AccountAddress>,
    #[structopt(short = "k")]
    /// if `to` account not exist on chain, must provide public_key of the account.
    public_key: Option<String>,

    #[structopt(short = "v")]
    amount: u128,
    #[structopt(
        short = "g",
        long = "max-gas",
        name = "max-gas-amount",
        default_value = "10000000",
        help = "max gas to use"
    )]
    max_gas_amount: u64,
    #[structopt(
        short = "p",
        long = "gas-price",
        name = "price of gas",
        default_value = "1",
        help = "gas price used"
    )]
    gas_price: u64,

    #[structopt(
        short = "t",
        long = "token-code",
        name = "token-code",
        help = "token's code, for example: 0x1::STC::STC, default is STC"
    )]
    token_code: Option<TokenCode>,

    #[structopt(
        short = "b",
        name = "blocking-mode",
        long = "blocking",
        help = "blocking wait txn mined"
    )]
    blocking: bool,
}

pub struct TransferCommand;

impl CommandAction for TransferCommand {
    type State = CliState;
    type GlobalOpt = StarcoinOpt;
    type Opt = TransferOpt;
    type ReturnItem = ExecuteResultView;

    fn run(
        &self,
        ctx: &ExecContext<Self::State, Self::GlobalOpt, Self::Opt>,
    ) -> Result<Self::ReturnItem> {
        let client = ctx.state().client();
        let opt = ctx.opt();
        let node_info = client.node_info()?;
        let sender = match opt.sender {
            Some(from) => client
                .account_get(from)?
                .ok_or_else(|| format_err!("Can not find WalletAccount by address: {}", from))?,
            None => client.account_default()?.ok_or_else(|| {
                format_err!("Can not find default account, Please input from account.")
            })?,
        };

        let chain_state_reader = RemoteStateReader::new(client)?;
        let account_state_reader = AccountStateReader::new(&chain_state_reader);

        let (receiver_address, receiver_auth_key) = match opt.receipt_identifier {
            None => {
                let receiver = opt.receiver.unwrap();
                let receiver_exist_on_chain = account_state_reader
                    .get_account_resource(&receiver)?
                    .is_some();
                let receiver_public_key = if receiver_exist_on_chain {
                    None
                } else {
                    let k = opt
                        .public_key
                        .as_ref()
                        .ok_or_else(|| {
                            format_err!(
                                "receiver account {} not exist on chain, please provide public_key",
                                receiver
                            )
                        })
                        .and_then(|pubkey_str| {
                            Ok(Ed25519PublicKey::from_encoded_string(pubkey_str)?)
                        })?;
                    Some(k)
                };
                let receiver_auth_key = receiver_public_key
                    .as_ref()
                    .map(|k| AuthenticationKey::ed25519(k));
                (receiver, receiver_auth_key)
            }
            Some(receipt_id) => match receipt_id {
                ReceiptIdentifier::V1(addr, auth_key) => (addr, auth_key),
            },
        };

        let account_resource = account_state_reader
            .get_account_resource(sender.address())?
            .ok_or_else(|| {
                format_err!(
                    "Can not find account on chain by address:{}",
                    sender.address()
                )
            })?;
        let token_code = opt
            .token_code
            .clone()
            .unwrap_or_else(|| STC_TOKEN_CODE.clone());
        let raw_txn = starcoin_executor::build_transfer_txn_by_token_type(
            sender.address,
            receiver_address,
            receiver_auth_key,
            account_resource.sequence_number(),
            opt.amount,
            opt.gas_price,
            opt.max_gas_amount,
            token_code,
            node_info.now_seconds + DEFAULT_EXPIRATION_TIME,
            ctx.state().net().chain_id(),
        );
        let txn = client.account_sign_txn(raw_txn)?;
        let txn_hash = txn.id();
        client.submit_transaction(txn)?;

        let mut output_view = ExecutionOutputView::new(txn_hash);

        if opt.blocking {
            let block = ctx.state().watch_txn(txn_hash)?.0;
            output_view.block_number = Some(block.header.number.0);
            output_view.block_id = Some(block.header.block_hash);
        }
        Ok(ExecuteResultView::Run(output_view))
    }
}
