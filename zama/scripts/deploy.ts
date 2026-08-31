import { ethers, fhevm } from "hardhat";

async function main() {
  await fhevm.initializeCLIApi();
  const [admin] = await ethers.getSigners();
  const requester = process.env.ZAMA_REQUESTER ?? admin.address;
  const settler = process.env.ZAMA_SETTLER ?? admin.address;
  const factory = await ethers.getContractFactory("ConfidentialRiskEngine");
  const engine = await factory.deploy(admin.address, requester, settler, admin.address);
  await engine.waitForDeployment();
  const capacity = process.env.ZAMA_CAPACITY ?? "1000000";
  const client = process.env.ZAMA_CLIENT ?? ethers.id("bridge-client");
  const address = await engine.getAddress();
  const cap = await fhevm.createEncryptedInput(address, admin.address).add64(BigInt(capacity)).encrypt();
  await (await engine.configureCapacity(cap.handles[0], cap.inputProof)).wait();
  const limit = await fhevm.createEncryptedInput(address, admin.address).add64(BigInt(capacity)).encrypt();
  await (await engine.configureClientLimit(client, limit.handles[0], limit.inputProof)).wait();
  console.log("ZAMA_ENGINE " + address);
  console.log("ZAMA_CLIENT " + client);
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
