import { ethers, fhevm } from "hardhat";

async function main() {
  await fhevm.initializeCLIApi();
  const rpc = process.env.ZAMA_RPC_URL ?? "http://127.0.0.1:8545";
  const engineAddress = process.env.ZAMA_ENGINE;
  const key = process.env.ZAMA_KEY;
  const method = process.env.ZAMA_METHOD;
  const args = (process.env.ZAMA_ARGS ?? "").split(",").filter(Boolean);
  if (!engineAddress || !key || !method) {
    throw new Error("ZAMA_ENGINE, ZAMA_KEY and ZAMA_METHOD are required");
  }
  const provider = new ethers.JsonRpcProvider(rpc);
  const signer = new ethers.Wallet(key, provider);
  const artifact = await import("../artifacts/contracts/ConfidentialRiskEngine.sol/ConfidentialRiskEngine.json");
  const engine = new ethers.Contract(engineAddress, artifact.abi, signer);

  if (method === "reserve") {
    const [reservationId, clientId, amount] = args;
    const encrypted = await fhevm
      .createEncryptedInput(engineAddress, signer.address)
      .add64(BigInt(amount))
      .encrypt();
    const tx = await engine.reserve(reservationId, clientId, encrypted.handles[0], encrypted.inputProof);
    await tx.wait();
    const handle = await engine.approvalHandle(reservationId);
    const approved = await fhevm.publicDecryptEbool(handle);
    console.log("ZAMA_RESULT " + JSON.stringify({ approved }));
    return;
  }

  const tx = await engine[method](args[0]);
  await tx.wait();
  console.log("ZAMA_RESULT " + JSON.stringify({ ok: true }));
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
