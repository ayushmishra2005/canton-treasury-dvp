import { expect } from "chai";
import { ethers, fhevm } from "hardhat";
import { FhevmType } from "@fhevm/hardhat-plugin";

async function encrypt64(contract: string, user: string, value: bigint) {
  return fhevm.createEncryptedInput(contract, user).add64(value).encrypt();
}

describe("ConfidentialRiskEngine (MOCK_FHE local execution)", function () {
  it("labels this suite as mock FHE execution", async function () {
    expect(fhevm.isMock).to.equal(true);
  });

  it("reserves within encrypted capacity and rejects overflow", async function () {
    const [admin, requester, settler] = await ethers.getSigners();
    const factory = await ethers.getContractFactory("ConfidentialRiskEngine");
    const engine = await factory.deploy(admin.address, requester.address, settler.address, admin.address);
    const address = await engine.getAddress();

    const cap = await encrypt64(address, admin.address, 1000n);
    await (await engine.connect(admin).configureCapacity(cap.handles[0], cap.inputProof)).wait();
    const limit = await encrypt64(address, admin.address, 400n);
    await (await engine.connect(admin).configureClientLimit(ethers.id("client-a"), limit.handles[0], limit.inputProof)).wait();

    const first = await encrypt64(address, requester.address, 250n);
    const firstId = ethers.id("res-1");
    await (await engine.connect(requester).reserve(firstId, ethers.id("client-a"), first.handles[0], first.inputProof)).wait();
    const firstOk = await fhevm.publicDecryptEbool(await engine.approvalHandle(firstId));
    expect(firstOk).to.equal(true);

    const overflow = await encrypt64(address, requester.address, 200n);
    const overflowId = ethers.id("res-overflow");
    await (await engine.connect(requester).reserve(overflowId, ethers.id("client-a"), overflow.handles[0], overflow.inputProof)).wait();
    const overflowOk = await fhevm.publicDecryptEbool(await engine.approvalHandle(overflowId));
    expect(overflowOk).to.equal(false);
  });

  it("rejects duplicate reservation identifiers", async function () {
    const [admin, requester, settler] = await ethers.getSigners();
    const factory = await ethers.getContractFactory("ConfidentialRiskEngine");
    const engine = await factory.deploy(admin.address, requester.address, settler.address, admin.address);
    const address = await engine.getAddress();
    const cap = await encrypt64(address, admin.address, 1000n);
    await (await engine.connect(admin).configureCapacity(cap.handles[0], cap.inputProof)).wait();
    const limit = await encrypt64(address, admin.address, 1000n);
    await (await engine.connect(admin).configureClientLimit(ethers.id("client-a"), limit.handles[0], limit.inputProof)).wait();
    const amount = await encrypt64(address, requester.address, 10n);
    const id = ethers.id("dup");
    await (await engine.connect(requester).reserve(id, ethers.id("client-a"), amount.handles[0], amount.inputProof)).wait();
    await expect(
      engine.connect(requester).reserve(id, ethers.id("client-a"), amount.handles[0], amount.inputProof)
    ).to.be.revertedWithCustomError(engine, "DuplicateReservation");
  });

  it("finalizes then redeems and rejects a second redeem", async function () {
    const [admin, requester, settler] = await ethers.getSigners();
    const factory = await ethers.getContractFactory("ConfidentialRiskEngine");
    const engine = await factory.deploy(admin.address, requester.address, settler.address, admin.address);
    const address = await engine.getAddress();
    const cap = await encrypt64(address, admin.address, 1000n);
    await (await engine.connect(admin).configureCapacity(cap.handles[0], cap.inputProof)).wait();
    const limit = await encrypt64(address, admin.address, 1000n);
    await (await engine.connect(admin).configureClientLimit(ethers.id("client-a"), limit.handles[0], limit.inputProof)).wait();
    const amount = await encrypt64(address, requester.address, 25n);
    const id = ethers.id("life");
    await (await engine.connect(requester).reserve(id, ethers.id("client-a"), amount.handles[0], amount.inputProof)).wait();
    expect(await fhevm.publicDecryptEbool(await engine.approvalHandle(id))).to.equal(true);
    await (await engine.connect(settler).finalize(id)).wait();
    await (await engine.connect(settler).redeem(id)).wait();
    await expect(engine.connect(settler).redeem(id)).to.be.revertedWithCustomError(engine, "WrongReservationStatus");
  });

  it("cancels a reservation before finalize and keeps epoch live exposure", async function () {
    const [admin, requester, settler] = await ethers.getSigners();
    const factory = await ethers.getContractFactory("ConfidentialRiskEngine");
    const engine = await factory.deploy(admin.address, requester.address, settler.address, admin.address);
    const address = await engine.getAddress();
    const cap = await encrypt64(address, admin.address, 1000n);
    await (await engine.connect(admin).configureCapacity(cap.handles[0], cap.inputProof)).wait();
    const limit = await encrypt64(address, admin.address, 1000n);
    await (await engine.connect(admin).configureClientLimit(ethers.id("client-a"), limit.handles[0], limit.inputProof)).wait();
    const amount = await encrypt64(address, requester.address, 30n);
    const id = ethers.id("cancel-me");
    await (await engine.connect(requester).reserve(id, ethers.id("client-a"), amount.handles[0], amount.inputProof)).wait();
    await (await engine.connect(settler).cancel(id)).wait();
    const before = await engine.epoch();
    await (await engine.connect(admin).rolloverEpoch()).wait();
    expect(await engine.epoch()).to.equal(before + 1n);
    await expect(engine.connect(settler).finalize(id)).to.be.revertedWithCustomError(engine, "WrongReservationStatus");
  });

  it("rejects a second finalize and keeps concurrent reservations within capacity", async function () {
    const [admin, requester, settler] = await ethers.getSigners();
    const factory = await ethers.getContractFactory("ConfidentialRiskEngine");
    const engine = await factory.deploy(admin.address, requester.address, settler.address, admin.address);
    const address = await engine.getAddress();
    const cap = await encrypt64(address, admin.address, 100n);
    await (await engine.connect(admin).configureCapacity(cap.handles[0], cap.inputProof)).wait();
    const limit = await encrypt64(address, admin.address, 100n);
    await (await engine.connect(admin).configureClientLimit(ethers.id("client-a"), limit.handles[0], limit.inputProof)).wait();
    const first = await encrypt64(address, requester.address, 60n);
    const second = await encrypt64(address, requester.address, 60n);
    const firstId = ethers.id("concurrent-1");
    const secondId = ethers.id("concurrent-2");
    await (await engine.connect(requester).reserve(firstId, ethers.id("client-a"), first.handles[0], first.inputProof)).wait();
    await (await engine.connect(requester).reserve(secondId, ethers.id("client-a"), second.handles[0], second.inputProof)).wait();
    expect(await fhevm.publicDecryptEbool(await engine.approvalHandle(firstId))).to.equal(true);
    expect(await fhevm.publicDecryptEbool(await engine.approvalHandle(secondId))).to.equal(false);
    await (await engine.connect(settler).finalize(firstId)).wait();
    await expect(engine.connect(settler).finalize(firstId)).to.be.revertedWithCustomError(engine, "WrongReservationStatus");
  });

  it("does not publicly decrypt amounts or limits in this mock suite", async function () {
    const [admin, requester, settler] = await ethers.getSigners();
    const factory = await ethers.getContractFactory("ConfidentialRiskEngine");
    const engine = await factory.deploy(admin.address, requester.address, settler.address, admin.address);
    const address = await engine.getAddress();
    const cap = await encrypt64(address, admin.address, 77n);
    await (await engine.connect(admin).configureCapacity(cap.handles[0], cap.inputProof)).wait();
    const iface = engine.interface;
    const parsed = iface.parseLog({
      topics: (await engine.queryFilter(engine.filters.CapacityConfigured())).at(-1)!.topics,
      data: (await engine.queryFilter(engine.filters.CapacityConfigured())).at(-1)!.data,
    });
    expect(JSON.stringify(parsed?.args)).to.not.include("77");
    void FhevmType.euint64;
    void requester;
    void settler;
  });
});
