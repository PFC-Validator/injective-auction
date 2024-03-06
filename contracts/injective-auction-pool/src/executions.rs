use std::str::FromStr;

use crate::helpers::query_current_auction;
use crate::state::{Auction, BIDDING_BALANCE, CONFIG, TREASURE_CHEST_CONTRACTS, UNSETTLED_AUCTION};
use crate::ContractError;
use cosmwasm_std::{
    coins, ensure, instantiate2_address, to_json_binary, Addr, BankMsg, Binary, CodeInfoResponse,
    Coin, CosmosMsg, Decimal, DepsMut, Env, MessageInfo, OverflowError, Response, Uint128, WasmMsg,
};
use injective_auction::auction::MsgBid;
use injective_auction::auction_pool::ExecuteMsg::TryBid;
use prost::Message;

const DAY_IN_SECONDS: u64 = 86400;

/// Joins the pool
pub(crate) fn join_pool(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    auction_round: u64,
    basket_value: Uint128,
) -> Result<Response, ContractError> {
    let config = CONFIG.load(deps.storage)?;
    let amount = cw_utils::must_pay(&info, &config.native_denom)?;

    let current_auction_round = query_current_auction(deps.as_ref())?
        .auction_round
        .ok_or(ContractError::CurrentAuctionQueryError)?;

    // prevents the user from joining the pool if the auction round is over
    if auction_round != current_auction_round {
        return Err(ContractError::InvalidAuctionRound {
            current_auction_round,
            auction_round,
        });
    }

    let mut messages = vec![];

    // mint the lp token and send it to the user
    let lp_subdenom = UNSETTLED_AUCTION.load(deps.storage)?.lp_subdenom;
    messages.push(config.token_factory_type.mint(
        env.contract.address.clone(),
        lp_subdenom.to_string().as_str(),
        amount,
    ));

    // send the minted lp token to the user
    let lp_denom = format!("factory/{}/{}", env.contract.address, lp_subdenom);
    messages.push(
        BankMsg::Send {
            to_address: info.sender.to_string(),
            amount: coins(amount.into(), lp_denom),
        }
        .into(),
    );

    // increase the balance that can be used for bidding
    BIDDING_BALANCE
        .update::<_, ContractError>(deps.storage, |balance| Ok(balance.checked_add(amount)?))?;

    // try to bid on the auction if possible
    messages.push(CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: env.contract.address.to_string(),
        msg: to_json_binary(&TryBid {
            auction_round,
            basket_value,
        })?,
        funds: vec![],
    }));

    Ok(Response::default().add_messages(messages).add_attributes(vec![
        ("action", "join_pool".to_string()),
        ("auction_round", auction_round.to_string()),
        ("sender", info.sender.to_string()),
        ("bid_amount", amount.to_string()),
    ]))
}

/// Exits the pool if the time is before T-1 day from the end of the auction.
pub(crate) fn exit_pool(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
) -> Result<Response, ContractError> {
    let current_auction_round_response = query_current_auction(deps.as_ref())?;

    //make sure the user sends a correct amount and denom to exit the pool
    let lp_denom = format!(
        "factory/{}/{}",
        env.contract.address,
        UNSETTLED_AUCTION.load(deps.storage)?.lp_subdenom
    );
    let amount = cw_utils::must_pay(&info, lp_denom.as_str())?;

    // TODO: change this to if statement to be consistent with the rest of the code
    // prevents the user from exiting the pool in the last day of the auction
    ensure!(
        DAY_IN_SECONDS
            > current_auction_round_response
                .auction_closing_time
                .ok_or(ContractError::CurrentAuctionQueryError)?
                .saturating_sub(env.block.time.seconds()),
        ContractError::PooledAuctionLocked
    );

    // subtract the amount of INJ to send from the bidding balance
    BIDDING_BALANCE
        .update::<_, ContractError>(deps.storage, |balance| Ok(balance.checked_sub(amount)?))?;

    let config = CONFIG.load(deps.storage)?;

    let mut messages = vec![];

    // burn the LP token and send the inj back to the user
    messages.push(config.token_factory_type.burn(
        env.contract.address.clone(),
        lp_denom.as_str(),
        amount,
    ));
    messages.push(
        BankMsg::Send {
            to_address: info.sender.to_string(),
            amount: coins(amount.into(), config.native_denom.clone()),
        }
        .into(),
    );

    Ok(Response::default()
        .add_messages(messages)
        .add_attributes(vec![("action", "exit_pool".to_string())]))
}

pub(crate) fn try_bid(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    auction_round: u64,
    basket_value: Uint128,
) -> Result<Response, ContractError> {
    // only whitelist addresses or the contract itself can bid on the auction
    let config = CONFIG.load(deps.storage)?;
    if info.sender != env.contract.address || !config.whitelisted_addresses.contains(&info.sender) {
        return Err(ContractError::Unauthorized {});
    }
    let current_auction_round_response = query_current_auction(deps.as_ref())?;
    let current_auction_round = current_auction_round_response
        .auction_round
        .ok_or(ContractError::CurrentAuctionQueryError)?;

    // prevents the contract from bidding on the wrong auction round
    if auction_round != current_auction_round {
        return Err(ContractError::InvalidAuctionRound {
            current_auction_round,
            auction_round,
        });
    }

    // prevents the contract from bidding if the contract is already the highest bidder
    if current_auction_round_response.highest_bidder == Some(env.contract.address.to_string()) {
        return Ok(Response::default()
            .add_attribute("action", "did_not_bid")
            .add_attribute("reason", "contract_is_already_the_highest_bidder"));
    }

    // calculate the minimum allowed bid to not be rejected by the auction module
    // minimum_allowed_bid = (highest_bid_amount * (1 + min_next_bid_increment_rate)) + 1
    let minimum_allowed_bid = current_auction_round_response
        .highest_bid_amount
        .unwrap_or(0.to_string())
        .parse::<Decimal>()?
        .checked_mul((Decimal::one().checked_add(config.min_next_bid_increment_rate))?)?
        .to_uint_ceil()
        .checked_add(Uint128::one())?;

    // prevents the contract from bidding if the minimum allowed bid is higher than bidding balance
    let bidding_balance: Uint128 = BIDDING_BALANCE.load(deps.storage)?;
    if minimum_allowed_bid > bidding_balance {
        return Ok(Response::default()
            .add_attribute("action", "did_not_bid")
            .add_attribute("reason", "minimum_allowed_bid_is_higher_than_bidding_balance"));
    }

    // prevents the contract from bidding if the returns are not high enough
    if basket_value * (Decimal::one() - config.min_return) > minimum_allowed_bid {
        return Ok(Response::default()
            .add_attribute("action", "did_not_bid")
            .add_attribute("reason", "basket_value_is_not_worth_bidding_for"));
    }

    // TODO: need to send some funds here?
    let message: CosmosMsg = CosmosMsg::Stargate {
        type_url: "/injective.auction.v1beta1.MsgBid".to_string(),
        value: {
            let msg = MsgBid {
                sender: env.contract.address.to_string(),
                bid_amount: Some(injective_auction::auction::Coin {
                    denom: config.native_denom,
                    amount: minimum_allowed_bid.to_string(),
                }),
                round: auction_round,
            };
            Binary(msg.encode_to_vec())
        },
    };

    Ok(Response::default()
        .add_message(message)
        .add_attribute("action", "try_bid".to_string())
        .add_attribute("amount", minimum_allowed_bid.to_string()))
}

pub fn settle_auction(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    auction_round: u64,
    auction_winner: String,
    auction_winning_bid: Uint128,
) -> Result<Response, ContractError> {
    // only whitelist addresses can settle the auction for now until the
    // contract can query the aunction module for a specific auction round
    let config = CONFIG.load(deps.storage)?;
    if !config.whitelisted_addresses.contains(&info.sender) {
        return Err(ContractError::Unauthorized {});
    }

    // prevents the contract from settling the wrong auction round
    let unsettled_auction = UNSETTLED_AUCTION.load(deps.storage)?;

    if auction_round != unsettled_auction.auction_round {
        return Err(ContractError::InvalidAuctionRound {
            current_auction_round: unsettled_auction.auction_round,
            auction_round,
        });
    }

    let current_auction_round_response = query_current_auction(deps.as_ref())?;
    let current_auction_round = current_auction_round_response
        .auction_round
        .ok_or(ContractError::CurrentAuctionQueryError)?;

    // prevents the contract from settling the auction if the auction round has not finished
    if current_auction_round == unsettled_auction.auction_round {
        return Err(ContractError::AuctionRoundHasNotFinished);
    }

    // the contract won the auction
    if auction_winner == env.contract.address.to_string() {
        // update LP subdenom for the next auction round (increment by 1)
        let new_subdenom = unsettled_auction.lp_subdenom.checked_add(1).ok_or(
            ContractError::OverflowError(OverflowError {
                operation: cosmwasm_std::OverflowOperation::Add,
                operand1: unsettled_auction.lp_subdenom.to_string(),
                operand2: 1.to_string(),
            }),
        )?;

        let basket = unsettled_auction.basket;
        let mut basket_fees = vec![];
        let mut basket_to_treasure_chest = vec![];

        // add the unused bidding balance to the basket to be redeemed later
        // TODO: should this be taxed though? if not, move after the for loop
        let remaining_bidding_balance =
            BIDDING_BALANCE.load(deps.storage)?.checked_sub(auction_winning_bid)?;

        if remaining_bidding_balance > Uint128::zero() {
            basket_to_treasure_chest.push(Coin {
                denom: config.native_denom.clone(),
                amount: remaining_bidding_balance,
            });
        }

        // split the basket, taking the rewards fees into account
        for coin in basket.iter() {
            let fee = coin.amount * config.rewards_fee;
            basket_fees.push(Coin {
                denom: coin.denom.clone(),
                amount: fee,
            });
            basket_to_treasure_chest.push(Coin {
                denom: coin.denom.clone(),
                amount: coin.amount.checked_sub(fee)?,
            });
        }

        // reset the bidding balance to 0 if we won, otherwise keep the balance for the next round
        BIDDING_BALANCE.save(deps.storage, &Uint128::zero())?;

        let mut messages: Vec<CosmosMsg> = vec![];

        // transfer corresponding tokens to the rewards fee address
        messages.push(CosmosMsg::Bank(BankMsg::Send {
            to_address: config.rewards_fee_addr.to_string(),
            amount: basket_fees,
        }));

        // instantiate a treasury chest contract and get the future contract address
        let creator = deps.api.addr_canonicalize(env.contract.address.as_str())?;
        let code_id = config.treasury_chest_code_id;

        let CodeInfoResponse {
            code_id: _,
            creator: _,
            checksum,
            ..
        } = deps.querier.query_wasm_code_info(code_id)?;

        let seed = format!(
            "{}{}{}",
            unsettled_auction.auction_round,
            info.sender.into_string(),
            env.block.height
        );
        let salt = Binary::from(seed.as_bytes());

        let treasure_chest_address =
            Addr::unchecked(&instantiate2_address(&checksum, &creator, &salt)?.to_string());

        let denom = format!("factory/{}/{}", env.contract.address, unsettled_auction.lp_subdenom);

        messages.push(CosmosMsg::Wasm(WasmMsg::Instantiate2 {
            admin: None,
            code_id,
            label: format!("Treasure chest for auction round {}", unsettled_auction.auction_round),
            msg: to_json_binary(&treasurechest::chest::InstantiateMsg {
                denom: config.native_denom.clone(),
                owner: env.contract.address.to_string(),
                notes: denom.clone(),
                token_factory: config.token_factory_type.to_string(),
                burn_it: Some(false),
            })?,
            funds: vec![],
            salt,
        }));

        TREASURE_CHEST_CONTRACTS.save(
            deps.storage,
            unsettled_auction.auction_round,
            &treasure_chest_address,
        )?;

        // transfer previous token factory's admin rights to the treasury chest contract
        messages.push(config.token_factory_type.change_admin(
            env.contract.address.clone(),
            &denom,
            treasure_chest_address.clone(),
        ));

        // create a new denom for the current auction round
        messages.push(
            config
                .token_factory_type
                .create_denom(env.contract.address, new_subdenom.to_string().as_str()),
        );

        Ok(Response::default()
            .add_messages(messages)
            .add_attribute("action", "settle_auction".to_string())
            .add_attribute("settled_action_round", unsettled_auction.auction_round.to_string())
            .add_attribute("treasure_chest_address", treasure_chest_address.to_string())
            .add_attribute("current_action_round", current_auction_round.to_string())
            .add_attribute("new_subdenom", new_subdenom.to_string()))
    }
    // the contract did NOT win the auction
    else {
        // save the current auction details to the contract state, keeping the previous LP subdenom
        UNSETTLED_AUCTION.save(
            deps.storage,
            &Auction {
                basket: current_auction_round_response
                    .amount
                    .iter()
                    .map(|coin| Coin {
                        amount: Uint128::from_str(&coin.amount)
                            .expect("Failed to parse coin amount"),
                        denom: coin.denom.clone(),
                    })
                    .collect(),
                auction_round: current_auction_round,
                lp_subdenom: unsettled_auction.lp_subdenom,
                closing_time: current_auction_round_response.auction_closing_time(),
            },
        )?;

        Ok(Response::default()
            .add_attribute("action", "settle_auction".to_string())
            .add_attribute("settled_action_round", unsettled_auction.auction_round.to_string())
            .add_attribute("current_action_round", current_auction_round.to_string()))
    }
}
