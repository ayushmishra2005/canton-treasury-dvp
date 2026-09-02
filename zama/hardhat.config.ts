import "@fhevm/hardhat-plugin";
import "@nomicfoundation/hardhat-ethers";
import "@nomicfoundation/hardhat-chai-matchers";
import "@typechain/hardhat";
import { HardhatUserConfig } from "hardhat/config";

function sepoliaAccounts(): string[] {
  const key = process.env.ZAMA_PRIVATE_KEY;
  if (!key) {
    return [];
  }
  return [key.startsWith("0x") ? key : `0x${key}`];
}

const config: HardhatUserConfig = {
  solidity: {
    version: "0.8.27",
    settings: {
      evmVersion: "cancun",
      viaIR: true,
      optimizer: { enabled: true, runs: 200 },
    },
  },
  typechain: {
    outDir: "typechain-types",
    target: "ethers-v6",
  },
  networks: {
    hardhat: { chainId: 31337 },
    localhost: { url: "http://127.0.0.1:8545", chainId: 31337 },
    sepolia: {
      url: process.env.ZAMA_RPC_URL ?? "https://ethereum-sepolia-rpc.publicnode.com",
      chainId: 11155111,
      accounts: sepoliaAccounts(),
    },
  },
  mocha: { timeout: 120000 },
};

export default config;
