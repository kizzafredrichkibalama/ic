use async_trait::async_trait;
use candid::CandidType;
use cycles_minting_canister::IcpXdrConversionRateCertifiedResponse;
use futures::join;
use ic_base_types::CanisterId;
use ic_nervous_system_common::{
    ledger::{ICRC1Ledger, IcpLedgerCanister as LedgerCanister},
    E8, UNITS_PER_PERMYRIAD,
};
use ic_nervous_system_runtime::{CdkRuntime, Runtime};
use ic_nervous_system_string::clamp_debug_len;
use ic_nns_constants::{CYCLES_MINTING_CANISTER_ID, LEDGER_CANISTER_ID as ICP_LEDGER_CANISTER_ID};
use ic_sns_swap::pb::v1::{
    GetDerivedStateRequest, GetDerivedStateResponse, GetInitRequest, GetInitResponse,
};
use icrc_ledger_types::icrc1::account::Account;
use mockall::automock;
use rust_decimal::Decimal;
use std::{
    fmt::Debug,
    marker::PhantomData,
    time::{Duration, SystemTime},
};

pub async fn try_get_icp_balance_valuation(account: Account) -> Result<Valuation, ValuationError> {
    let timestamp = now();

    try_get_balance_valuation_factors(
        account,
        &mut LedgerCanister::new(ICP_LEDGER_CANISTER_ID),
        &mut IcpsPerIcpClient {},
        &mut new_standard_xdrs_per_icp_client::<CdkRuntime>(),
    )
    .await
    .map(|valuation_factors| Valuation {
        token: Token::Icp,
        account,
        timestamp,
        valuation_factors,
    })
}

pub async fn try_get_sns_token_balance_valuation(
    account: Account,
    sns_ledger_canister_id: CanisterId,
    swap_canister_id: CanisterId,
) -> Result<Valuation, ValuationError> {
    let timestamp = now();

    try_get_balance_valuation_factors(
        account,
        &mut LedgerCanister::new(sns_ledger_canister_id),
        &mut IcpsPerSnsTokenClient::<CdkRuntime>::new(swap_canister_id),
        &mut new_standard_xdrs_per_icp_client::<CdkRuntime>(),
    )
    .await
    .map(|valuation_factors| Valuation {
        token: Token::SnsToken,
        account,
        timestamp,
        valuation_factors,
    })
}

fn now() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_nanos(ic_cdk::api::time())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Token {
    Icp,

    /// The native token of the SNS.
    SnsToken,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Valuation {
    pub token: Token,
    pub account: Account,
    pub timestamp: SystemTime,
    pub valuation_factors: ValuationFactors,
}

impl Valuation {
    pub fn to_xdr(&self) -> Decimal {
        self.valuation_factors.to_xdr()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ValuationFactors {
    pub tokens: Decimal,
    pub icps_per_token: Decimal,
    pub xdrs_per_icp: Decimal,
}

impl ValuationFactors {
    pub fn to_xdr(&self) -> Decimal {
        let Self {
            tokens,
            icps_per_token,
            xdrs_per_icp,
        } = self;

        tokens * icps_per_token * xdrs_per_icp
    }
}

/// Returns a valuation in XDR of the current balance in account.
///
/// # Arguments
/// * `account` - Where funds are held. In the case of an SNS's treasury, this is the default
///   subaccount of the SNS governance canister.
/// * `icrc1_client` - Reads the balance of `account`.
/// * `icps_per_token_client` - For conversion to ICP from whatever token the icrc1_client deals in.
///   Of course, in the case of ICP, this conversion is trivial, and is implemented by the
///   IcpsPerIcpClient in this crate.
/// * `xdrs_per_icp_client` - Supplies the ICP -> XDR conversion rate. This is probably the most
///   interesting of the clients used. A object suitable for production can be constructed by
///   calling new_standard_xdrs_per_icp_client::<DfnRuntime> with zero arguments.
async fn try_get_balance_valuation_factors(
    account: Account,
    icrc1_client: &mut dyn ICRC1Ledger,
    icps_per_token_client: &mut dyn IcpsPerTokenClient,
    xdrs_per_icp_client: &mut dyn XdrsPerIcpClient,
) -> Result<ValuationFactors, ValuationError> {
    // Fetch the three ingredients:
    //
    //     1. balance
    //     2. token -> ICP
    //     3. ICP -> XDR
    //
    // No await here. Instead, we use join (right after this).
    let account_balance_request = icrc1_client.account_balance(account);
    let icps_per_token_request = icps_per_token_client.get();
    let xdrs_per_icp_request = xdrs_per_icp_client.get();

    // Make all (3) requests (concurrently).
    let (account_balance_response, icps_per_token_response, xdrs_per_icp_response) = join!(
        account_balance_request,
        icps_per_token_request,
        xdrs_per_icp_request,
    );

    // Unwrap/forward errors to the caller.
    let account_balance_response = account_balance_response.map_err(|err| {
        ValuationError::new_external(format!("Unable to obtain balance from ledger: {:?}", err))
    })?;
    let icps_per_token_response = icps_per_token_response.map_err(|err| {
        ValuationError::new_external(format!("Unable to determine ICPs per token: {:?}", err))
    })?;
    let xdrs_per_icp_response = xdrs_per_icp_response.map_err(|err| {
        ValuationError::new_external(format!("Unable to obtain XDR per ICP: {:?}", err))
    })?;

    // Extract and interpret the data we actually care about from the (Ok) responses.
    let tokens = Decimal::from(account_balance_response.get_e8s()) / Decimal::from(E8);
    let icps_per_token = icps_per_token_response;
    let xdrs_per_icp = xdrs_per_icp_response;

    // Compose the fetched/interpretted data (i.e. multiply them) to construct the final result.
    Ok(ValuationFactors {
        tokens,
        icps_per_token,
        xdrs_per_icp,
    })
}

// ValuationError

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValuationError {
    pub species: ValuationErrorSpecies,

    /// Human-readable. Ideally, explains what could not be done, proximate and prior causes, and
    /// includes breadcrumbs to help the reader figure out how to get what they wanted.
    pub message: String,
}

impl ValuationError {
    fn new_external(message: String) -> Self {
        Self {
            message,
            species: ValuationErrorSpecies::External,
        }
    }

    fn new_mismatch(message: String) -> Self {
        Self {
            message,
            species: ValuationErrorSpecies::Mismatch,
        }
    }

    fn new_arithmetic(message: String) -> Self {
        Self {
            message,
            species: ValuationErrorSpecies::Arithmetic,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValuationErrorSpecies {
    /// Needed data from another canister, but was not able to get a reply.
    External,

    /// Got a reply from another canister, but the reply did not contain the needed data. This could
    /// be due to talking to an incompatible (more advanced?) version of the canister.
    Mismatch,

    /// E.g. overflow, underflow, divide by zero, etc.
    Arithmetic,
}

// Traits

#[automock]
#[async_trait]
trait IcpsPerTokenClient: Send {
    async fn get(&mut self) -> Result<Decimal, ValuationError>;
}

#[automock]
#[async_trait]
trait XdrsPerIcpClient: Send {
    async fn get(&mut self) -> Result<Decimal, ValuationError>;
}

// Trait Implementations Suitable For Production.

struct IcpsPerIcpClient {}

#[async_trait]
impl IcpsPerTokenClient for IcpsPerIcpClient {
    async fn get(&mut self) -> Result<Decimal, ValuationError> {
        Ok(Decimal::from(1))
    }
}

struct IcpsPerSnsTokenClient<MyRuntime: Runtime + Send + Sync> {
    swap_canister_id: CanisterId,
    _runtime: PhantomData<MyRuntime>,
}

impl<MyRuntime: Runtime + Send + Sync> IcpsPerSnsTokenClient<MyRuntime> {
    pub fn new(swap_canister_id: CanisterId) -> Self {
        Self {
            swap_canister_id,
            _runtime: Default::default(),
        }
    }

    async fn fetch_icps_per_sns_token(&self) -> Result<Decimal, ValuationError> {
        // Call get_derived_state and get_init.
        let (get_derived_state_response, get_init_response) = join!(
            self.call(GetDerivedStateRequest {}),
            self.call(GetInitRequest {}),
        );

        // Unwrap responses, and if there were errors, return Err.
        let get_derived_state_response = get_derived_state_response?;
        let get_init_response = get_init_response?;

        // Read the relevant fields out of the responses.
        let buyer_total_icp_e8s =
            get_derived_state_response
                .buyer_total_icp_e8s
                .ok_or_else(|| {
                    ValuationError::new_mismatch(format!(
                        "Response from swap ({}) get_derived_state call did not \
                         contain sns_tokens_per_icp: {:#?}",
                        self.swap_canister_id, get_derived_state_response,
                    ))
                })?;
        let sns_token_e8s = get_init_response
            .init
            .ok_or_else(|| {
                ValuationError::new_mismatch(format!(
                    "init field in GetInitResponse from swap canister {} empty.",
                    self.swap_canister_id,
                ))
            })?
            .sns_token_e8s
            .ok_or_else(|| {
                ValuationError::new_mismatch(format!(
                    "init.sns_token_e8es field in GetInitResponse from swap canister {} empty.",
                    self.swap_canister_id,
                ))
            })?;

        // Prepare to divide (for final result) by first converting to Decimal.
        let buyer_total_icp_e8s = Decimal::from(buyer_total_icp_e8s);
        let sns_token_e8s = Decimal::from(sns_token_e8s);

        buyer_total_icp_e8s
            .checked_div(sns_token_e8s)
            // Swap is supposed to ensure that 0 is not returned. Therefore, this is just defense in
            // depth.
            .ok_or_else(|| {
                ValuationError::new_arithmetic(format!(
                    "Unable to determine the price of an SNS token (with respect to ICP), \
                     because the sns_token_e8s field in the GetInitResponse from swap canister \
                     ({}) was zero.",
                    self.swap_canister_id,
                ))
            })
    }

    async fn call<MyRequest>(
        &self,
        request: MyRequest,
    ) -> Result<MyRequest::MyResponse, ValuationError>
    where
        MyRequest: Request + Debug + Clone + Sync,
        <MyRequest as Request>::MyResponse: CandidType,
    {
        call::<_, MyRuntime>(self.swap_canister_id, request.clone())
            .await
            .map_err(|err| {
                ValuationError::new_external(format!(
                    "Unable to determine ICPs per SNS token, because calling swap canister \
                     {} failed. Request:\n{}\nerr: {:?}",
                    self.swap_canister_id,
                    clamp_debug_len(request, /* max_len = */ 100),
                    err,
                ))
            })
    }
}

#[async_trait]
impl<R: Runtime + Send + Sync> IcpsPerTokenClient for IcpsPerSnsTokenClient<R> {
    async fn get(&mut self) -> Result<Decimal, ValuationError> {
        self.fetch_icps_per_sns_token().await
    }
}

// Here, "standard" just means that it is appropriate for production use.
fn new_standard_xdrs_per_icp_client<MyRuntime: Runtime + Send + Sync>() -> impl XdrsPerIcpClient {
    struct CmcBased30DayMovingAverageXdrsPerIcpClient<MyRuntime: Runtime + Send + Sync> {
        _runtime: PhantomData<MyRuntime>,
    }

    #[async_trait]
    impl<MyRuntime: Runtime + Send + Sync> XdrsPerIcpClient
        for CmcBased30DayMovingAverageXdrsPerIcpClient<MyRuntime>
    {
        async fn get(&mut self) -> Result<Decimal, ValuationError> {
            let (response,): (IcpXdrConversionRateCertifiedResponse,) =
                MyRuntime::call_with_cleanup(
                    CYCLES_MINTING_CANISTER_ID,
                    // This is not in the cmc.did file (yet).
                    "get_average_icp_xdr_conversion_rate",
                    ((),),
                )
                .await
                .map_err(|err| {
                    ValuationError::new_external(format!(
                        "Unable to determine XDRs per ICP, because the cycles minting canister \
                         did not reply to a get_average_icp_xdr_conversion_rate call: {:?}",
                        err,
                    ))
                })?;

            // No need to validate the cerificate in response, because query is not used in this
            // case (specifically, canister A in subnet X is calling (another) canister B in
            // (another) subnet Y).

            let xdr_per_icp =
                Decimal::from(response.data.xdr_permyriad_per_icp) * *UNITS_PER_PERMYRIAD;

            Ok(xdr_per_icp)
        }
    }

    CmcBased30DayMovingAverageXdrsPerIcpClient::<MyRuntime> {
        _runtime: Default::default(),
    }
}

// Generic Helpers (could be moved to more general place).

/// Associates a request type with method_name and response type.
///
/// This is based on the pattern where
///
/// ```candid
/// service : {
///     greet : (GreetRequest) -> (GreetResponse);
/// }
/// ```
///
/// Once you know one of the three pieces, you know the other two.
///
/// By implementing this trait, you are telling fn call how to deduce the method name and response
/// type from the request/argument type, which reduces quite a fair amount of redundancy.
// TODO: Implement #[derive(Request)]. This would replace the hand-crafted implementations below.
trait Request: CandidType + Send {
    const METHOD_NAME: &'static str;
    type MyResponse: for<'a> candid::Deserialize<'a>;
}

impl Request for GetDerivedStateRequest {
    const METHOD_NAME: &'static str = "get_derived_state";
    type MyResponse = GetDerivedStateResponse;
}

impl Request for GetInitRequest {
    const METHOD_NAME: &'static str = "get_init";
    type MyResponse = GetInitResponse;
}

async fn call<MyRequest, MyRuntime>(
    destination_canister_id: CanisterId,
    request: MyRequest,
) -> Result<MyRequest::MyResponse, (i32, String)>
where
    MyRequest: Request + Sync,
    MyRuntime: Runtime,
    <MyRequest as Request>::MyResponse: CandidType,
{
    let (response,): (MyRequest::MyResponse,) =
        MyRuntime::call_with_cleanup(destination_canister_id, MyRequest::METHOD_NAME, (request,))
            .await?;

    Ok(response)
}

#[cfg(test)]
mod tests;
