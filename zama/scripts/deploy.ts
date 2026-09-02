import { ethers, fhevm } from "hardhat";
import { ConfidentialRiskEngine__factory } from "../typechain-types/factories/contracts/ConfidentialRiskEngine__factory";

async function main() {
  await fhevm.initializeCLIApi();
  const network = await ethers.provider.getNetwork();
  const chainId = Number(network.chainId);
  const [admin] = await ethers.getSigners();
  const requester = process.env.ZAMA_REQUESTER ?? admin.address;
  const settler = process.env.ZAMA_SETTLER ?? admin.address;
  const pauser = process.env.ZAMA_PAUSER ?? admin.address;
  const start = await ethers.provider.getBalance(admin.address);
  const fee = await ethers.provider.getFeeData();
  const gasPrice = fee.gasPrice ?? 0n;
  const factory = new ConfidentialRiskEngine__factory(admin);
  const deployTx = await factory.getDeployTransaction(admin.address, requester, settler, pauser);
  const deployGas = await ethers.provider.estimateGas(deployTx);
  const deployCost = deployGas * gasPrice;
  console.log("ZAMA_CHAIN_ID " + chainId);
  console.log("ZAMA_MOCK_FHE " + fhevm.isMock);
  console.log("ZAMA_WALLET " + admin.address);
  console.log("ZAMA_START_WEI " + start.toString());
  console.log("ZAMA_GAS_PRICE_WEI " + gasPrice.toString());
  console.log("ZAMA_DEPLOY_GAS_ESTIMATE " + deployGas.toString());
  console.log("ZAMA_DEPLOY_COST_ESTIMATE_WEI " + deployCost.toString());
  if (chainId === 11155111 && fhevm.isMock) {
    throw new Error("mock FHE must be disabled on Sepolia");
  }
  if (start <= deployCost) {
    throw new Error("wallet cannot cover estimated deploy gas");
  }

  const engine = await factory.deploy(admin.address, requester, settler, pauser);
  const deployed = await engine.deploymentTransaction()?.wait();
  if (!deployed) {
    throw new Error("deployment transaction missing");
  }
  const address = await engine.getAddress();
  console.log("ZAMA_ENGINE " + address);
  console.log("ZAMA_DEPLOY_TX " + deployed.hash);
  console.log("ZAMA_DEPLOY_GAS " + (deployed.gasUsed ?? 0n).toString());
  console.log("ZAMA_ROLE_GRANT_TX " + deployed.hash);
  console.log("ZAMA_POLICY_ADMIN " + admin.address);
  console.log("ZAMA_REQUESTER " + requester);
  console.log("ZAMA_SETTLER " + settler);
  console.log("ZAMA_PAUSER " + pauser);

  const capacity = process.env.ZAMA_CAPACITY ?? "200000000000";
  const client = process.env.ZAMA_CLIENT ?? ethers.id("bridge-client");
  const cap = await fhevm.createEncryptedInput(address, admin.address).add64(BigInt(capacity)).encrypt();
  const capTx = await (await engine.configureCapacity(cap.handles[0], cap.inputProof)).wait();
  if (!capTx) {
    throw new Error("capacity transaction missing");
  }
  console.log("ZAMA_CAPACITY_TX " + capTx.hash);
  console.log("ZAMA_CAPACITY_GAS " + capTx.gasUsed.toString());
  const limit = await fhevm.createEncryptedInput(address, admin.address).add64(BigInt(capacity)).encrypt();
  const clientTx = await (await engine.configureClientLimit(client, limit.handles[0], limit.inputProof)).wait();
  if (!clientTx) {
    throw new Error("client configuration transaction missing");
  }
  console.log("ZAMA_CLIENT " + client);
  console.log("ZAMA_CLIENT_TX " + clientTx.hash);
  console.log("ZAMA_CLIENT_GAS " + clientTx.gasUsed.toString());
  const end = await ethers.provider.getBalance(admin.address);
  console.log("ZAMA_END_WEI " + end.toString());
  console.log("ZAMA_SPENT_WEI " + (start - end).toString());
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
