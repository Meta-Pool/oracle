use anyhow::{anyhow, bail};
use async_trait::async_trait;
use candid::{CandidType, Decode, Deserialize, Encode, Principal};
use ic_base_types::PrincipalId;
use icp_ledger::AccountIdentifier;

mod generated;

use crate::governance::generated::{
    AddHotKey, ChangeAutoStakeMaturity, Command, Command_1, Configure, Disburse, DissolveState, ListNeurons,
    ClaimOrRefreshNeuronFromAccount, ClaimOrRefreshNeuronFromAccountResponse, IncreaseDissolveDelay, ListNeuronsResponse, ManageNeuron, ManageNeuronResponse,
    Neuron, NeuronId, NeuronIdOrSubaccount, Operation, Result_1, SpawnResponse, Split,
};

const ICP_FEE: u64 = 10_000;

#[async_trait]
pub trait Service {
    // Disburse all disburseable neurons to the target address
    // 1. Fetch a list of any disburseable neurons from the governance service
    // 2. Disburse any disburseable neurons into the deposits canister
    async fn disburse_neurons(&self, now: u64, address: &AccountIdentifier) -> anyhow::Result<()>;

    // Apply the given list of neuron splits, adding the given hotkeys to each new neuron, and
    // starting the new neurons dissolving.
    //
    // These steps are handled previously in canister updates (for better atomicity), and passed in.
    // 1. Query dissolving neurons total & pending total, to calculate dissolving target from the
    //    Deposits service
    // 2. Calculate which staking neurons to split and how much

    // 3. Split & dissolve new neurons as needed
    async fn split_new_withdrawal_neurons(
        &self,
        neurons_to_split: Vec<(u64, u64)>,
    ) -> anyhow::Result<()>;

    async fn claim_neuron(&self, controller: Option<Principal>, memo: u64) -> anyhow::Result<u64>;
    async fn increase_neuron_delay(&self, neuron_id: u64, additional_dissolve_delay_seconds: u32) -> anyhow::Result<()>;
    async fn add_hotkey(&self, neuron_id: u64, key: Principal) -> anyhow::Result<()>;
    async fn enable_auto_merge_maturity(&self, neuron_id: u64) -> anyhow::Result<()>;

    // Calculate the governance canister's account id for creating new neurons
    fn account_id(&self) -> anyhow::Result<AccountIdentifier>;
}

pub struct Agent<'a> {
    pub agent: &'a ic_agent::Agent,
    pub canister_id: Principal,
}

impl Agent<'_> {
    // TODO: Load the args etc here from a local candid file
    async fn list_neurons(&self) -> anyhow::Result<Vec<Neuron>> {
        let response = self
            .agent
            .update(&self.canister_id, "list_neurons")
            .with_arg(&Encode!(&ListNeurons {
                neuron_ids: vec![],
                include_neurons_readable_by_caller: true,
            })?)
            .call_and_wait()
            .await?;

        let result =
            Decode!(response.as_slice(), ListNeuronsResponse).map_err(|err| anyhow!(err))?;
        Ok(result.full_neurons)
    }

    async fn manage_neuron(
        &self,
        id: u64,
        command: Command,
    ) -> anyhow::Result<ManageNeuronResponse> {
        let response = self
            .agent
            .update(&self.canister_id, "manage_neuron")
            .with_arg(&Encode!(&ManageNeuron {
                id: None,
                command: Some(command),
                neuron_id_or_subaccount: Some(NeuronIdOrSubaccount::NeuronId(NeuronId { id })),
            })?)
            .call_and_wait()
            .await?;

        Decode!(response.as_slice(), ManageNeuronResponse).map_err(|err| anyhow!(err))
    }
}

#[async_trait]
impl Service for Agent<'_> {
    async fn disburse_neurons(&self, now: u64, address: &AccountIdentifier) -> anyhow::Result<()> {
        let neurons = self.list_neurons().await?;
        for n in neurons.iter() {
            let Some(NeuronId { id }) = n.id else {
                continue;
            };
            let Some(DissolveState::WhenDissolvedTimestampSeconds(dissolved_at)) = n.dissolve_state else {
                continue;
            };
            if now < dissolved_at {
                continue;
            }
            self.manage_neuron(
                id,
                Command::Disburse(Disburse {
                    to_account: Some(generated::AccountIdentifier{ hash: address.hash.try_into()? }),
                    amount: None, // all
                }),
            )
            .await?;
        }
        Ok(())
    }

    async fn split_new_withdrawal_neurons(
        &self,
        neurons_to_split: Vec<(u64, u64)>,
    ) -> anyhow::Result<()> {
        for (id, amount_e8s) in neurons_to_split.iter() {
            let ManageNeuronResponse{
                command: Some(Command_1::Split(SpawnResponse {
                    created_neuron_id: Some(NeuronId {
                        id: new_id,
                    }),
                }))
            } = self.manage_neuron(
                id.clone(),
                Command::Split(Split { amount_e8s: amount_e8s.clone() }),
            )
            .await? else {
                bail!("Unexpected response when splitting neuron {}", id)
            };

            // Start the new neuron dissolving
            self.manage_neuron(
                new_id,
                Command::Configure(Configure {
                    operation: Some(Operation::StartDissolving),
                }),
            )
            .await?;
        }
        Ok(())
    }

    async fn claim_neuron(&self, controller: Option<Principal>, memo: u64) -> anyhow::Result<u64> {
        let response = self
            .agent
            .update(&self.canister_id, "claim_or_refresh_neuron_from_account")
            .with_arg(&Encode!(&ClaimOrRefreshNeuronFromAccount { controller, memo })?)
            .call_and_wait()
            .await?;

        let result = Decode!(response.as_slice(), ClaimOrRefreshNeuronFromAccountResponse).map_err(|err| anyhow!(err))?;
        let Some(inner) = result.result else {
            bail!("Unexpected result claiming neuron, memo: {}", memo);
        };
        match inner {
            Result_1::Error(err) => bail!("Error claiming neuron, memo: {}, err: {}", memo, err.error_message),
            Result_1::NeuronId(NeuronId { id }) => Ok(id),
        }
    }

    async fn increase_neuron_delay(&self, neuron_id: u64, additional_dissolve_delay_seconds: u32) -> anyhow::Result<()> {
            self.manage_neuron(
                neuron_id,
                Command::Configure(Configure {
                    operation: Some(Operation::IncreaseDissolveDelay(IncreaseDissolveDelay {
                        additional_dissolve_delay_seconds,
                    })),
                }),
            )
            .await?;
            Ok(())
    }

    async fn add_hotkey(&self, neuron_id: u64, key: Principal) -> anyhow::Result<()> {
            self.manage_neuron(
                neuron_id,
                Command::Configure(Configure {
                    operation: Some(Operation::AddHotKey(AddHotKey {
                        new_hot_key: Some(key),
                    })),
                }),
            )
            .await?;
            Ok(())
    }

    async fn enable_auto_merge_maturity(&self, neuron_id: u64) -> anyhow::Result<()> {
            self.manage_neuron(
                neuron_id,
                Command::Configure(Configure {
                    operation: Some(Operation::ChangeAutoStakeMaturity(ChangeAutoStakeMaturity {
                        requested_setting_for_auto_stake_maturity: true,
                    })),
                }),
            )
            .await?;
            Ok(())
    }

    fn account_id(&self) -> anyhow::Result<AccountIdentifier> {
        PrincipalId::try_from(self.canister_id.as_slice())
            .map(|p| AccountIdentifier::new(p, None))
            .map_err(|err| anyhow!(err))
    }
}
