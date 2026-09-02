import { ethers, fhevm } from "hardhat";

async function main() {
  await fhevm.initializeCLIApi();
  const network = await ethers.provider.getNetwork();
  const chainId = Number(network.chainId);
  if (chainId !== 11155111) {
    throw new Error("expected Sepolia chain ID 11155111, got " + chainId);
  }
  if (fhevm.isMock) {
    throw new Error("mock FHE is enabled on Sepolia");
  }
  console.log("ZAMA_CHAIN_ID " + chainId);
  console.log("ZAMA_MOCK_FHE " + fhevm.isMock);
  console.log("ZAMA_COPROCESSOR ethereum-sepolia");
  console.log("ZAMA_FHE_CHECK_OK");
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
