import { ethers, fhevm } from "hardhat";
import { ConfidentialRiskEngine } from "../typechain-types/contracts/ConfidentialRiskEngine";
import { ConfidentialRiskEngine__factory } from "../typechain-types/factories/contracts/ConfidentialRiskEngine__factory";

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
  const engine = ConfidentialRiskEngine__factory.connect(engineAddress, signer);

  if (method === "status") {
    const status = await engine.reservationStatus(args[0]);
    console.log("ZAMA_RESULT " + JSON.stringify({ status: Number(status) }));
    return;
  }

  if (method === "approved") {
    const handle = await engine.approvalHandle(args[0]);
    const approved = await fhevm.publicDecryptEbool(handle);
    console.log("ZAMA_RESULT " + JSON.stringify({ approved }));
    return;
  }

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

  if (method === "finalize" || method === "cancel" || method === "redeem") {
    await settle(engine, method, args[0]);
    console.log("ZAMA_RESULT " + JSON.stringify({ ok: true }));
    return;
  }

  throw new Error("unsupported Zama method: " + method);
}

async function settle(engine: ConfidentialRiskEngine, method: "finalize" | "cancel" | "redeem", id: string) {
  if (method === "finalize") {
    await (await engine.finalize(id)).wait();
    return;
  }
  if (method === "cancel") {
    await (await engine.cancel(id)).wait();
    return;
  }
  await (await engine.redeem(id)).wait();
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
