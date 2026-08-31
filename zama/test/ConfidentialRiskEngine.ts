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

  it("rejected reservation finalize and redeem do not free another client's exposure", async function () {
    const [admin, requester, settler] = await ethers.getSigners();
    const factory = await ethers.getContractFactory("ConfidentialRiskEngine");
    const engine = await factory.deploy(admin.address, requester.address, settler.address, admin.address);
    const address = await engine.getAddress();
    const cap = await encrypt64(address, admin.address, 150n);
    await (await engine.connect(admin).configureCapacity(cap.handles[0], cap.inputProof)).wait();
    const limit = await encrypt64(address, admin.address, 150n);
    await (await engine.connect(admin).configureClientLimit(ethers.id("client-a"), limit.handles[0], limit.inputProof)).wait();

    const aAmt = await encrypt64(address, requester.address, 100n);
    const aId = ethers.id("res-A");
    await (await engine.connect(requester).reserve(aId, ethers.id("client-a"), aAmt.handles[0], aAmt.inputProof)).wait();
    expect(await fhevm.publicDecryptEbool(await engine.approvalHandle(aId))).to.equal(true);
    await (await engine.connect(settler).finalize(aId)).wait();

    const bAmt = await encrypt64(address, requester.address, 100n);
    const bId = ethers.id("res-B");
    await (await engine.connect(requester).reserve(bId, ethers.id("client-a"), bAmt.handles[0], bAmt.inputProof)).wait();
    expect(await fhevm.publicDecryptEbool(await engine.approvalHandle(bId))).to.equal(false);

    await (await engine.connect(settler).finalize(bId)).wait();
    await (await engine.connect(settler).redeem(bId)).wait();
    expect(await engine.reservationStatus(bId)).to.equal(4n);

    const cAmt = await encrypt64(address, requester.address, 100n);
    const cId = ethers.id("res-C");
    await (await engine.connect(requester).reserve(cId, ethers.id("client-a"), cAmt.handles[0], cAmt.inputProof)).wait();
    expect(await fhevm.publicDecryptEbool(await engine.approvalHandle(cId))).to.equal(false);
  });

  it("rejected reservation cancel and repeated settle calls leave live exposure unchanged", async function () {
    const [admin, requester, settler] = await ethers.getSigners();
    const factory = await ethers.getContractFactory("ConfidentialRiskEngine");
    const engine = await factory.deploy(admin.address, requester.address, settler.address, admin.address);
    const address = await engine.getAddress();
    const cap = await encrypt64(address, admin.address, 150n);
    await (await engine.connect(admin).configureCapacity(cap.handles[0], cap.inputProof)).wait();
    const limit = await encrypt64(address, admin.address, 150n);
    await (await engine.connect(admin).configureClientLimit(ethers.id("client-a"), limit.handles[0], limit.inputProof)).wait();

    const liveAmt = await encrypt64(address, requester.address, 100n);
    const liveId = ethers.id("res-live");
    await (await engine.connect(requester).reserve(liveId, ethers.id("client-a"), liveAmt.handles[0], liveAmt.inputProof)).wait();
    await (await engine.connect(settler).finalize(liveId)).wait();

    const rejectedAmt = await encrypt64(address, requester.address, 100n);
    const rejectedId = ethers.id("res-rejected");
    await (await engine.connect(requester).reserve(rejectedId, ethers.id("client-a"), rejectedAmt.handles[0], rejectedAmt.inputProof)).wait();
    expect(await fhevm.publicDecryptEbool(await engine.approvalHandle(rejectedId))).to.equal(false);
    await (await engine.connect(settler).cancel(rejectedId)).wait();
    await expect(engine.connect(settler).cancel(rejectedId)).to.be.revertedWithCustomError(engine, "WrongReservationStatus");
    await expect(engine.connect(settler).finalize(rejectedId)).to.be.revertedWithCustomError(engine, "WrongReservationStatus");

    const again = await encrypt64(address, requester.address, 100n);
    const againId = ethers.id("res-after-cancel");
    await (await engine.connect(requester).reserve(againId, ethers.id("client-a"), again.handles[0], again.inputProof)).wait();
    expect(await fhevm.publicDecryptEbool(await engine.approvalHandle(againId))).to.equal(false);
  });

  it("zero and overflowing reservation amounts stay rejected without changing later capacity", async function () {
    const [admin, requester, settler] = await ethers.getSigners();
    const factory = await ethers.getContractFactory("ConfidentialRiskEngine");
    const engine = await factory.deploy(admin.address, requester.address, settler.address, admin.address);
    const address = await engine.getAddress();
    const cap = await encrypt64(address, admin.address, 50n);
    await (await engine.connect(admin).configureCapacity(cap.handles[0], cap.inputProof)).wait();
    const limit = await encrypt64(address, admin.address, 50n);
    await (await engine.connect(admin).configureClientLimit(ethers.id("client-a"), limit.handles[0], limit.inputProof)).wait();

    const zero = await encrypt64(address, requester.address, 0n);
    const zeroId = ethers.id("res-zero");
    await (await engine.connect(requester).reserve(zeroId, ethers.id("client-a"), zero.handles[0], zero.inputProof)).wait();
    expect(await fhevm.publicDecryptEbool(await engine.approvalHandle(zeroId))).to.equal(false);
    await (await engine.connect(settler).finalize(zeroId)).wait();
    await (await engine.connect(settler).redeem(zeroId)).wait();

    const overflow = await encrypt64(address, requester.address, 2n ** 64n - 1n);
    const overflowId = ethers.id("res-u64-max");
    await (await engine.connect(requester).reserve(overflowId, ethers.id("client-a"), overflow.handles[0], overflow.inputProof)).wait();
    expect(await fhevm.publicDecryptEbool(await engine.approvalHandle(overflowId))).to.equal(false);
    await (await engine.connect(settler).cancel(overflowId)).wait();

    const ok = await encrypt64(address, requester.address, 50n);
    const okId = ethers.id("res-exact");
    await (await engine.connect(requester).reserve(okId, ethers.id("client-a"), ok.handles[0], ok.inputProof)).wait();
    expect(await fhevm.publicDecryptEbool(await engine.approvalHandle(okId))).to.equal(true);
  });

  it("reapplying or changing a client limit preserves reserved and active usage", async function () {
    const [admin, requester, settler] = await ethers.getSigners();
    const factory = await ethers.getContractFactory("ConfidentialRiskEngine");
    const engine = await factory.deploy(admin.address, requester.address, settler.address, admin.address);
    const address = await engine.getAddress();
    const cap = await encrypt64(address, admin.address, 200n);
    await (await engine.connect(admin).configureCapacity(cap.handles[0], cap.inputProof)).wait();
    const limit100 = await encrypt64(address, admin.address, 100n);
    await (await engine.connect(admin).configureClientLimit(ethers.id("client-a"), limit100.handles[0], limit100.inputProof)).wait();

    const first = await encrypt64(address, requester.address, 80n);
    const firstId = ethers.id("res-usage");
    await (await engine.connect(requester).reserve(firstId, ethers.id("client-a"), first.handles[0], first.inputProof)).wait();
    expect(await fhevm.publicDecryptEbool(await engine.approvalHandle(firstId))).to.equal(true);
    await (await engine.connect(settler).finalize(firstId)).wait();

    const sameLimit = await encrypt64(address, admin.address, 100n);
    await (await engine.connect(admin).configureClientLimit(ethers.id("client-a"), sameLimit.handles[0], sameLimit.inputProof)).wait();
    const second = await encrypt64(address, requester.address, 80n);
    const secondId = ethers.id("res-usage-2");
    await (await engine.connect(requester).reserve(secondId, ethers.id("client-a"), second.handles[0], second.inputProof)).wait();
    expect(await fhevm.publicDecryptEbool(await engine.approvalHandle(secondId))).to.equal(false);

    const raised = await encrypt64(address, admin.address, 160n);
    await (await engine.connect(admin).configureClientLimit(ethers.id("client-a"), raised.handles[0], raised.inputProof)).wait();
    const third = await encrypt64(address, requester.address, 80n);
    const thirdId = ethers.id("res-usage-3");
    await (await engine.connect(requester).reserve(thirdId, ethers.id("client-a"), third.handles[0], third.inputProof)).wait();
    expect(await fhevm.publicDecryptEbool(await engine.approvalHandle(thirdId))).to.equal(true);

    const lowered = await encrypt64(address, admin.address, 100n);
    await (await engine.connect(admin).configureClientLimit(ethers.id("client-a"), lowered.handles[0], lowered.inputProof)).wait();
    const fourth = await encrypt64(address, requester.address, 10n);
    const fourthId = ethers.id("res-usage-4");
    await (await engine.connect(requester).reserve(fourthId, ethers.id("client-a"), fourth.handles[0], fourth.inputProof)).wait();
    expect(await fhevm.publicDecryptEbool(await engine.approvalHandle(fourthId))).to.equal(false);

    await (await engine.connect(settler).finalize(thirdId)).wait();
    await (await engine.connect(settler).redeem(thirdId)).wait();
    const afterRedeem = await encrypt64(address, requester.address, 10n);
    const afterRedeemId = ethers.id("res-usage-5");
    await (await engine.connect(requester).reserve(afterRedeemId, ethers.id("client-a"), afterRedeem.handles[0], afterRedeem.inputProof)).wait();
    expect(await fhevm.publicDecryptEbool(await engine.approvalHandle(afterRedeemId))).to.equal(true);
    await (await engine.connect(settler).cancel(afterRedeemId)).wait();
  });

  it("epoch rollover does not erase reserved or active exposure", async function () {
    const [admin, requester, settler] = await ethers.getSigners();
    const factory = await ethers.getContractFactory("ConfidentialRiskEngine");
    const engine = await factory.deploy(admin.address, requester.address, settler.address, admin.address);
    const address = await engine.getAddress();
    const cap = await encrypt64(address, admin.address, 100n);
    await (await engine.connect(admin).configureCapacity(cap.handles[0], cap.inputProof)).wait();
    const limit = await encrypt64(address, admin.address, 100n);
    await (await engine.connect(admin).configureClientLimit(ethers.id("client-a"), limit.handles[0], limit.inputProof)).wait();
    const reserved = await encrypt64(address, requester.address, 40n);
    const reservedId = ethers.id("res-epoch-reserved");
    await (await engine.connect(requester).reserve(reservedId, ethers.id("client-a"), reserved.handles[0], reserved.inputProof)).wait();
    const active = await encrypt64(address, requester.address, 40n);
    const activeId = ethers.id("res-epoch-active");
    await (await engine.connect(requester).reserve(activeId, ethers.id("client-a"), active.handles[0], active.inputProof)).wait();
    await (await engine.connect(settler).finalize(activeId)).wait();
    const before = await engine.epoch();
    await (await engine.connect(admin).rolloverEpoch()).wait();
    expect(await engine.epoch()).to.equal(before + 1n);
    const extra = await encrypt64(address, requester.address, 40n);
    const extraId = ethers.id("res-epoch-extra");
    await (await engine.connect(requester).reserve(extraId, ethers.id("client-a"), extra.handles[0], extra.inputProof)).wait();
    expect(await fhevm.publicDecryptEbool(await engine.approvalHandle(extraId))).to.equal(false);
    await (await engine.connect(settler).cancel(reservedId)).wait();
    const afterCancel = await encrypt64(address, requester.address, 40n);
    const afterCancelId = ethers.id("res-epoch-after-cancel");
    await (await engine.connect(requester).reserve(afterCancelId, ethers.id("client-a"), afterCancel.handles[0], afterCancel.inputProof)).wait();
    expect(await fhevm.publicDecryptEbool(await engine.approvalHandle(afterCancelId))).to.equal(true);
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
