use abscissa_core::clap::Parser;
use abscissa_core::{config::Override, Command, FrameworkErrorKind, Runnable};

use core::time::Duration;
use ibc::{
    applications::transfer::Amount,
    core::{
        ics02_client::client_state::ClientState,
        ics24_host::identifier::{ChainId, ChannelId, PortId},
    },
    events::IbcEvent,
};
use ibc_relayer::chain::handle::ChainHandle;
use ibc_relayer::chain::requests::{
    IncludeProof, QueryChannelRequest, QueryClientStateRequest, QueryConnectionRequest, QueryHeight,
};
use ibc_relayer::{
    config::Config,
    transfer::{build_and_send_transfer_messages, TransferOptions},
};

use crate::cli_utils::ChainHandlePair;
use crate::conclude::{exit_with_unrecoverable_error, Output};
use crate::error::Error;
use crate::prelude::*;

#[derive(Clone, Command, Debug, Parser, PartialEq)]
pub struct TxIcs20MsgTransferCmd {
    #[clap(
        long = "dst-chain",
        required = true,
        value_name = "DST_CHAIN_ID",
        help_heading = "REQUIRED",
        help = "Identifier of the destination chain"
    )]
    dst_chain_id: ChainId,

    #[clap(
        long = "src-chain",
        required = true,
        value_name = "SRC_CHAIN_ID",
        help_heading = "REQUIRED",
        help = "Identifier of the source chain"
    )]
    src_chain_id: ChainId,

    #[clap(
        long = "src-port",
        required = true,
        value_name = "SRC_PORT_ID",
        help_heading = "REQUIRED",
        help = "Identifier of the source port"
    )]
    src_port_id: PortId,

    #[clap(
        long = "src-channel",
        visible_alias = "src-chan",
        required = true,
        value_name = "SRC_CHANNEL_ID",
        help_heading = "REQUIRED",
        help = "Identifier of the source channel"
    )]
    src_channel_id: ChannelId,

    #[clap(
        long = "amount",
        required = true,
        value_name = "AMOUNT",
        help_heading = "REQUIRED",
        help = "Amount of coins (samoleans, by default) to send (e.g. `100000`)"
    )]
    amount: Amount,

    #[clap(
        long = "timeout-height-offset",
        default_value = "0",
        value_name = "TIMEOUT_HEIGHT_OFFSET",
        help = "Timeout in number of blocks since current"
    )]
    timeout_height_offset: u64,

    #[clap(
        long = "timeout-seconds",
        default_value = "0",
        value_name = "TIMEOUT_SECONDS",
        help = "Timeout in seconds since current"
    )]
    timeout_seconds: u64,

    #[clap(
        long = "receiver",
        value_name = "RECEIVER",
        help = "The account address on the destination chain which will receive the tokens. If omitted, the relayer's wallet on the destination chain will be used"
    )]
    receiver: Option<String>,

    #[clap(
        long = "denom",
        value_name = "DENOM",
        help = "Denomination of the coins to send",
        default_value = "samoleans"
    )]
    denom: String,

    #[clap(
        long = "number-msgs",
        value_name = "NUMBER_MSGS",
        help = "Number of messages to send"
    )]
    number_msgs: Option<usize>,

    #[clap(
        long = "key-name",
        value_name = "KEY_NAME",
        help = "Use the given signing key name (default: `key_name` config)"
    )]
    key_name: Option<String>,
}

impl Override<Config> for TxIcs20MsgTransferCmd {
    fn override_config(&self, mut config: Config) -> Result<Config, abscissa_core::FrameworkError> {
        let src_chain_config = config.find_chain_mut(&self.src_chain_id).ok_or_else(|| {
            FrameworkErrorKind::ComponentError.context(format!(
                "missing configuration for source chain '{}'",
                self.src_chain_id
            ))
        })?;

        if let Some(ref key_name) = self.key_name {
            src_chain_config.key_name = key_name.to_string();
        }

        Ok(config)
    }
}

impl TxIcs20MsgTransferCmd {
    fn validate_options(
        &self,
        config: &Config,
    ) -> Result<TransferOptions, Box<dyn std::error::Error>> {
        config.find_chain(&self.src_chain_id).ok_or_else(|| {
            format!(
                "missing configuration for source chain '{}'",
                self.src_chain_id
            )
        })?;

        config.find_chain(&self.dst_chain_id).ok_or_else(|| {
            format!(
                "missing configuration for destination chain '{}'",
                self.dst_chain_id
            )
        })?;

        let denom = self.denom.clone();

        let number_msgs = self.number_msgs.unwrap_or(1);
        if number_msgs == 0 {
            return Err("number of messages should be greater than zero".into());
        }

        let opts = TransferOptions {
            packet_src_port_id: self.src_port_id.clone(),
            packet_src_channel_id: self.src_channel_id.clone(),
            amount: self.amount,
            denom,
            receiver: self.receiver.clone(),
            timeout_height_offset: self.timeout_height_offset,
            timeout_duration: Duration::from_secs(self.timeout_seconds),
            number_msgs,
        };

        Ok(opts)
    }
}

impl Runnable for TxIcs20MsgTransferCmd {
    fn run(&self) {
        let config = app_config();

        let opts = match self.validate_options(&config) {
            Err(err) => Output::error(err).exit(),
            Ok(result) => result,
        };

        debug!("Message: {:?}", opts);

        let chains = ChainHandlePair::spawn(&config, &self.src_chain_id, &self.dst_chain_id)
            .unwrap_or_else(exit_with_unrecoverable_error);

        // Double check that channels and chain identifiers match.
        // To do this, fetch from the source chain the channel end, then the associated connection
        // end, and then the underlying client state; finally, check that this client is verifying
        // headers for the destination chain.
        let (channel_end_src, _) = chains
            .src
            .query_channel(
                QueryChannelRequest {
                    port_id: opts.packet_src_port_id.clone(),
                    channel_id: opts.packet_src_channel_id.clone(),
                    height: QueryHeight::Latest,
                },
                IncludeProof::No,
            )
            .unwrap_or_else(exit_with_unrecoverable_error);
        if !channel_end_src.is_open() {
            Output::error(format!(
                "the requested port/channel ('{}'/'{}') on chain id '{}' is in state '{}'; expected 'open' state",
                opts.packet_src_port_id,
                opts.packet_src_channel_id,
                self.src_chain_id,
                channel_end_src.state
            ))
                .exit();
        }

        let conn_id = match channel_end_src.connection_hops.first() {
            None => {
                Output::error(format!(
                    "could not retrieve the connection hop underlying port/channel '{}'/'{}' on chain '{}'",
                    opts.packet_src_port_id, opts.packet_src_channel_id, self.src_chain_id
                ))
                    .exit();
            }
            Some(cid) => cid,
        };

        let (conn_end, _) = chains
            .src
            .query_connection(
                QueryConnectionRequest {
                    connection_id: conn_id.clone(),
                    height: QueryHeight::Latest,
                },
                IncludeProof::No,
            )
            .unwrap_or_else(exit_with_unrecoverable_error);

        debug!("connection hop underlying the channel: {:?}", conn_end);

        let (src_chain_client_state, _) = chains
            .src
            .query_client_state(
                QueryClientStateRequest {
                    client_id: conn_end.client_id().clone(),
                    height: QueryHeight::Latest,
                },
                IncludeProof::No,
            )
            .unwrap_or_else(exit_with_unrecoverable_error);

        debug!(
            "client state underlying the channel: {:?}",
            src_chain_client_state
        );

        if src_chain_client_state.chain_id() != self.dst_chain_id {
            Output::error(
                format!("the requested port/channel ('{}'/'{}') provides a path from chain '{}' to \
                 chain '{}' (not to the destination chain '{}'). Bailing due to mismatching arguments.",
                        opts.packet_src_port_id, opts.packet_src_channel_id,
                        self.src_chain_id,
                        src_chain_client_state.chain_id(), self.dst_chain_id)).exit();
        }

        // Checks pass, build and send the tx
        let res: Result<Vec<IbcEvent>, Error> =
            build_and_send_transfer_messages(&chains.src, &chains.dst, &opts)
                .map_err(Error::transfer);

        match res {
            Ok(ev) => Output::success(ev).exit(),
            Err(e) => Output::error(format!("{}", e)).exit(),
        }
    }
}

#[cfg(test)]
mod tests {
    use ibc::{
        applications::transfer::Amount,
        core::ics24_host::identifier::{ChainId, ChannelId, PortId},
    };

    use super::TxIcs20MsgTransferCmd;

    use abscissa_core::clap::Parser;
    use std::str::FromStr;

    #[test]
    fn test_ft_transfer_required_only() {
        assert_eq!(
            TxIcs20MsgTransferCmd {
                dst_chain_id: ChainId::from_string("chain_receiver"),
                src_chain_id: ChainId::from_string("chain_sender"),
                src_port_id: PortId::from_str("port_sender").unwrap(),
                src_channel_id: ChannelId::from_str("channel_sender").unwrap(),
                amount: Amount::from(42),
                timeout_height_offset: 0,
                timeout_seconds: 0,
                receiver: None,
                denom: "samoleans".to_owned(),
                number_msgs: None,
                key_name: None
            },
            TxIcs20MsgTransferCmd::parse_from(&[
                "test",
                "--dst-chain",
                "chain_receiver",
                "--src-chain",
                "chain_sender",
                "--src-port",
                "port_sender",
                "--src-channel",
                "channel_sender",
                "--amount",
                "42"
            ])
        )
    }

    #[test]
    fn test_ft_transfer_aliases() {
        assert_eq!(
            TxIcs20MsgTransferCmd {
                dst_chain_id: ChainId::from_string("chain_receiver"),
                src_chain_id: ChainId::from_string("chain_sender"),
                src_port_id: PortId::from_str("port_sender").unwrap(),
                src_channel_id: ChannelId::from_str("channel_sender").unwrap(),
                amount: Amount::from(42),
                timeout_height_offset: 0,
                timeout_seconds: 0,
                receiver: None,
                denom: "samoleans".to_owned(),
                number_msgs: None,
                key_name: None
            },
            TxIcs20MsgTransferCmd::parse_from(&[
                "test",
                "--dst-chain",
                "chain_receiver",
                "--src-chain",
                "chain_sender",
                "--src-port",
                "port_sender",
                "--src-chan",
                "channel_sender",
                "--amount",
                "42"
            ])
        )
    }

    #[test]
    fn test_ft_transfer_denom() {
        assert_eq!(
            TxIcs20MsgTransferCmd {
                dst_chain_id: ChainId::from_string("chain_receiver"),
                src_chain_id: ChainId::from_string("chain_sender"),
                src_port_id: PortId::from_str("port_sender").unwrap(),
                src_channel_id: ChannelId::from_str("channel_sender").unwrap(),
                amount: Amount::from(42),
                timeout_height_offset: 0,
                timeout_seconds: 0,
                receiver: None,
                denom: "my_denom".to_owned(),
                number_msgs: None,
                key_name: None
            },
            TxIcs20MsgTransferCmd::parse_from(&[
                "test",
                "--dst-chain",
                "chain_receiver",
                "--src-chain",
                "chain_sender",
                "--src-port",
                "port_sender",
                "--src-channel",
                "channel_sender",
                "--amount",
                "42",
                "--denom",
                "my_denom"
            ])
        )
    }

    #[test]
    fn test_ft_transfer_key_name() {
        assert_eq!(
            TxIcs20MsgTransferCmd {
                dst_chain_id: ChainId::from_string("chain_receiver"),
                src_chain_id: ChainId::from_string("chain_sender"),
                src_port_id: PortId::from_str("port_sender").unwrap(),
                src_channel_id: ChannelId::from_str("channel_sender").unwrap(),
                amount: Amount::from(42),
                timeout_height_offset: 0,
                timeout_seconds: 0,
                receiver: None,
                denom: "samoleans".to_owned(),
                number_msgs: None,
                key_name: Some("key_name".to_owned())
            },
            TxIcs20MsgTransferCmd::parse_from(&[
                "test",
                "--dst-chain",
                "chain_receiver",
                "--src-chain",
                "chain_sender",
                "--src-port",
                "port_sender",
                "--src-channel",
                "channel_sender",
                "--amount",
                "42",
                "--key-name",
                "key_name"
            ])
        )
    }

    #[test]
    fn test_ft_transfer_number_msgs() {
        assert_eq!(
            TxIcs20MsgTransferCmd {
                dst_chain_id: ChainId::from_string("chain_receiver"),
                src_chain_id: ChainId::from_string("chain_sender"),
                src_port_id: PortId::from_str("port_sender").unwrap(),
                src_channel_id: ChannelId::from_str("channel_sender").unwrap(),
                amount: Amount::from(42),
                timeout_height_offset: 0,
                timeout_seconds: 0,
                receiver: None,
                denom: "samoleans".to_owned(),
                number_msgs: Some(21),
                key_name: None
            },
            TxIcs20MsgTransferCmd::parse_from(&[
                "test",
                "--dst-chain",
                "chain_receiver",
                "--src-chain",
                "chain_sender",
                "--src-port",
                "port_sender",
                "--src-channel",
                "channel_sender",
                "--amount",
                "42",
                "--number-msgs",
                "21"
            ])
        )
    }

    #[test]
    fn test_ft_transfer_receiver() {
        assert_eq!(
            TxIcs20MsgTransferCmd {
                dst_chain_id: ChainId::from_string("chain_receiver"),
                src_chain_id: ChainId::from_string("chain_sender"),
                src_port_id: PortId::from_str("port_sender").unwrap(),
                src_channel_id: ChannelId::from_str("channel_sender").unwrap(),
                amount: Amount::from(42),
                timeout_height_offset: 0,
                timeout_seconds: 0,
                receiver: Some("receiver_addr".to_owned()),
                denom: "samoleans".to_owned(),
                number_msgs: None,
                key_name: None
            },
            TxIcs20MsgTransferCmd::parse_from(&[
                "test",
                "--dst-chain",
                "chain_receiver",
                "--src-chain",
                "chain_sender",
                "--src-port",
                "port_sender",
                "--src-channel",
                "channel_sender",
                "--amount",
                "42",
                "--receiver",
                "receiver_addr"
            ])
        )
    }

    #[test]
    fn test_ft_transfer_timeout_height_offset() {
        assert_eq!(
            TxIcs20MsgTransferCmd {
                dst_chain_id: ChainId::from_string("chain_receiver"),
                src_chain_id: ChainId::from_string("chain_sender"),
                src_port_id: PortId::from_str("port_sender").unwrap(),
                src_channel_id: ChannelId::from_str("channel_sender").unwrap(),
                amount: Amount::from(42),
                timeout_height_offset: 21,
                timeout_seconds: 0,
                receiver: None,
                denom: "samoleans".to_owned(),
                number_msgs: None,
                key_name: None
            },
            TxIcs20MsgTransferCmd::parse_from(&[
                "test",
                "--dst-chain",
                "chain_receiver",
                "--src-chain",
                "chain_sender",
                "--src-port",
                "port_sender",
                "--src-channel",
                "channel_sender",
                "--amount",
                "42",
                "--timeout-height-offset",
                "21"
            ])
        )
    }

    #[test]
    fn test_ft_transfer_timeout_seconds() {
        assert_eq!(
            TxIcs20MsgTransferCmd {
                dst_chain_id: ChainId::from_string("chain_receiver"),
                src_chain_id: ChainId::from_string("chain_sender"),
                src_port_id: PortId::from_str("port_sender").unwrap(),
                src_channel_id: ChannelId::from_str("channel_sender").unwrap(),
                amount: Amount::from(42),
                timeout_height_offset: 0,
                timeout_seconds: 21,
                receiver: None,
                denom: "samoleans".to_owned(),
                number_msgs: None,
                key_name: None
            },
            TxIcs20MsgTransferCmd::parse_from(&[
                "test",
                "--dst-chain",
                "chain_receiver",
                "--src-chain",
                "chain_sender",
                "--src-port",
                "port_sender",
                "--src-channel",
                "channel_sender",
                "--amount",
                "42",
                "--timeout-seconds",
                "21"
            ])
        )
    }

    #[test]
    fn test_ft_transfer_no_amount() {
        assert!(TxIcs20MsgTransferCmd::try_parse_from(&[
            "test",
            "--dst-chain",
            "chain_receiver",
            "--src-chain",
            "chain_sender",
            "--src-port",
            "port_sender",
            "--src-channel",
            "channel_sender"
        ])
        .is_err())
    }

    #[test]
    fn test_ft_transfer_no_sender_channel() {
        assert!(TxIcs20MsgTransferCmd::try_parse_from(&[
            "test",
            "--dst-chain",
            "chain_receiver",
            "--src-chain",
            "chain_sender",
            "--src-port",
            "port_sender",
            "--amount",
            "42"
        ])
        .is_err())
    }

    #[test]
    fn test_ft_transfer_no_sender_port() {
        assert!(TxIcs20MsgTransferCmd::try_parse_from(&[
            "test",
            "--dst-chain",
            "chain_receiver",
            "--src-chain",
            "chain_sender",
            "--src-channel",
            "channel_sender",
            "--amount",
            "42"
        ])
        .is_err())
    }

    #[test]
    fn test_ft_transfer_no_sender_chain() {
        assert!(TxIcs20MsgTransferCmd::try_parse_from(&[
            "test",
            "--dst-chain",
            "chain_receiver",
            "--src-port",
            "port_sender",
            "--src-channel",
            "channel_sender",
            "--amount",
            "42"
        ])
        .is_err())
    }

    #[test]
    fn test_ft_transfer_no_receiver_chain() {
        assert!(TxIcs20MsgTransferCmd::try_parse_from(&[
            "test",
            "--src-chain",
            "chain_sender",
            "--src-port",
            "port_sender",
            "--src-channel",
            "channel_sender",
            "--amount",
            "42"
        ])
        .is_err())
    }
}
