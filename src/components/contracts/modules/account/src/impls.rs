use crate::{storage::*, App, Config};
use config::abci::global_cfg::CFG;
use enterprise_web3::{
    AllowancesKey, ALLOWANCES, BALANCE_MAP, TOTAL_ISSUANCE, WEB3_SERVICE_START_HEIGHT,
};
use fp_core::{account::SmartAccount, context::Context};
use fp_storage::BorrowMut;
use fp_traits::account::AccountAsset;
use fp_types::crypto::Address;
use primitive_types::{H160, U256};
use ruc::*;
impl<C: Config> AccountAsset<Address> for App<C> {
    fn total_issuance(ctx: &Context) -> U256 {
        TotalIssuance::get(&ctx.state.read()).unwrap_or_default()
    }
    fn account_of(
        ctx: &Context,
        who: &Address,
        height: Option<u64>,
    ) -> Option<SmartAccount> {
        let version = height.unwrap_or(0);
        if version == 0 {
            AccountStore::get(&ctx.state.read(), who)
        } else {
            AccountStore::get_ver(&ctx.state.read(), who, version)
        }
    }

    fn balance(ctx: &Context, who: &Address) -> U256 {
        Self::account_of(ctx, who, None).unwrap_or_default().balance
    }

    fn reserved_balance(ctx: &Context, who: &Address) -> U256 {
        Self::account_of(ctx, who, None)
            .unwrap_or_default()
            .reserved
    }

    fn nonce(ctx: &Context, who: &Address) -> U256 {
        Self::account_of(ctx, who, None).unwrap_or_default().nonce
    }

    fn inc_nonce(ctx: &Context, who: &Address) -> Result<U256> {
        let mut sa = Self::account_of(ctx, who, None).c(d!("account does not exist"))?;
        sa.nonce = sa.nonce.saturating_add(U256::one());
        AccountStore::insert(ctx.state.write().borrow_mut(), who, &sa).map(|()| sa.nonce)
    }

    fn transfer(
        ctx: &Context,
        sender: &Address,
        dest: &Address,
        balance: U256,
    ) -> Result<()> {
        if balance.is_zero() || sender == dest {
            return Ok(());
        }

        let mut from_account =
            Self::account_of(ctx, sender, None).c(d!("sender does not exist"))?;
        let mut to_account = Self::account_of(ctx, dest, None).unwrap_or_default();
        from_account.balance = from_account
            .balance
            .checked_sub(balance)
            .c(d!("insufficient balance"))?;
        to_account.balance = to_account
            .balance
            .checked_add(balance)
            .c(d!("balance overflow"))?;
        AccountStore::insert(ctx.state.write().borrow_mut(), sender, &from_account)?;
        AccountStore::insert(ctx.state.write().borrow_mut(), dest, &to_account)?;

        if CFG.enable_enterprise_web3
            && ctx.header.height as u64 > *WEB3_SERVICE_START_HEIGHT
        {
            let mut balance_map = BALANCE_MAP.lock().c(d!())?;
            let sender_slice: &[u8] = sender.as_ref();
            let sender_h160 = H160::from_slice(&sender_slice[4..24]);

            let to_slice: &[u8] = dest.as_ref();
            let to_h160 = H160::from_slice(&to_slice[4..24]);

            balance_map.insert(sender_h160, from_account.balance);
            balance_map.insert(to_h160, to_account.balance);
        }

        Ok(())
    }

    fn mint(ctx: &Context, target: &Address, balance: U256) -> Result<()> {
        if balance.is_zero() {
            return Ok(());
        }

        let mut target_account = Self::account_of(ctx, target, None).unwrap_or_default();
        target_account.balance = target_account
            .balance
            .checked_add(balance)
            .c(d!("balance overflow"))?;

        AccountStore::insert(ctx.state.write().borrow_mut(), target, &target_account)?;

        let issuance = Self::total_issuance(ctx)
            .checked_add(balance)
            .c(d!("issuance overflow"))?;
        TotalIssuance::put(ctx.state.write().borrow_mut(), &issuance)?;

        if CFG.enable_enterprise_web3
            && ctx.header.height as u64 > *WEB3_SERVICE_START_HEIGHT
        {
            let mut balance_map = BALANCE_MAP.lock().c(d!())?;
            let target_slice: &[u8] = target.as_ref();
            let target_h160 = H160::from_slice(&target_slice[4..24]);
            balance_map.insert(target_h160, target_account.balance);
            set_total_issuance(issuance)?;
        }

        Ok(())
    }

    fn burn(ctx: &Context, target: &Address, balance: U256) -> Result<()> {
        if balance.is_zero() {
            return Ok(());
        }

        let mut target_account = Self::account_of(ctx, target, None)
            .c(d!(format!("account: {target} does not exist")))?;
        target_account.balance = target_account
            .balance
            .checked_sub(balance)
            .c(d!("insufficient balance"))?;

        AccountStore::insert(ctx.state.write().borrow_mut(), target, &target_account)?;

        let issuance = Self::total_issuance(ctx)
            .checked_sub(balance)
            .c(d!("insufficient issuance"))?;
        TotalIssuance::put(ctx.state.write().borrow_mut(), &issuance)?;

        if CFG.enable_enterprise_web3
            && ctx.header.height as u64 > *WEB3_SERVICE_START_HEIGHT
        {
            let mut balance_map = BALANCE_MAP.lock().c(d!())?;
            let target_slice: &[u8] = target.as_ref();
            let target_h160 = H160::from_slice(&target_slice[4..24]);
            balance_map.insert(target_h160, target_account.balance);
            set_total_issuance(issuance)?;
        }

        Ok(())
    }

    fn withdraw(ctx: &Context, who: &Address, value: U256) -> Result<()> {
        if value.is_zero() {
            return Ok(());
        }

        let mut sa = Self::account_of(ctx, who, None).c(d!("account does not exist"))?;
        sa.balance = sa
            .balance
            .checked_sub(value)
            .c(d!("insufficient balance"))?;
        sa.reserved = sa
            .reserved
            .checked_add(value)
            .c(d!("reserved balance overflow"))?;

        AccountStore::insert(ctx.state.write().borrow_mut(), who, &sa)?;

        if CFG.enable_enterprise_web3
            && ctx.header.height as u64 > *WEB3_SERVICE_START_HEIGHT
        {
            let mut balance_map = BALANCE_MAP.lock().c(d!())?;
            let target_slice: &[u8] = who.as_ref();
            let target_h160 = H160::from_slice(&target_slice[4..24]);

            balance_map.insert(target_h160, sa.balance);
        }

        Ok(())
    }

    fn refund(ctx: &Context, who: &Address, value: U256) -> Result<()> {
        if value.is_zero() {
            return Ok(());
        }

        let mut sa = Self::account_of(ctx, who, None).c(d!("account does not exist"))?;
        sa.reserved = sa
            .reserved
            .checked_sub(value)
            .c(d!("insufficient reserved balance"))?;
        sa.balance = sa.balance.checked_add(value).c(d!("balance overflow"))?;
        AccountStore::insert(ctx.state.write().borrow_mut(), who, &sa)?;

        if CFG.enable_enterprise_web3
            && ctx.header.height as u64 > *WEB3_SERVICE_START_HEIGHT
        {
            let mut balance_map = BALANCE_MAP.lock().c(d!())?;
            let target_slice: &[u8] = who.as_ref();
            let target_h160 = H160::from_slice(&target_slice[4..24]);

            balance_map.insert(target_h160, sa.balance);
        }

        Ok(())
    }
    fn allowance(ctx: &Context, owner: &Address, spender: &Address) -> U256 {
        Allowances::get(&ctx.state.read(), owner, spender).unwrap_or_default()
    }
    fn approve(
        ctx: &Context,
        owner: &Address,
        owner_address: H160,
        spender: &Address,
        spender_address: H160,
        amount: U256,
    ) -> Result<()> {
        Allowances::insert(ctx.state.write().borrow_mut(), owner, spender, &amount)?;
        if CFG.enable_enterprise_web3
            && ctx.header.height as u64 > *WEB3_SERVICE_START_HEIGHT
        {
            let mut allowances = ALLOWANCES.lock().c(d!())?;

            allowances.push((
                AllowancesKey {
                    owner_address,
                    spender_address,
                },
                amount,
            ));
        }
        Ok(())
    }
}

fn set_total_issuance(issuance: U256) -> Result<()> {
    let mut total_issuance = TOTAL_ISSUANCE.lock().c(d!())?;
    *total_issuance = Some(issuance);
    Ok(())
}
