import { expect } from "chai";
import { FakerGeneratorFactory } from "../builder/FakerGeneratorFactory";
import {
  aMessageDataPlayerLoginWs,
  aMessagePlayerLoginWs,
  aMessageWs,
} from "../builder/builders";
import { MessageDataPlayerLogin } from "../builder/model/horizon/data/MessageDataPlayerLogin";

describe("FakerGeneratorFactory", () => {
  it("should use the env seed if provided", () => {
    const faker = FakerGeneratorFactory.getInstance();

    expect(faker).to.exist;
    expect(typeof faker.internet.email()).to.equal("string");
  });

  it("build a ws message for player with ddurieux login", () => {
    const playerLoginWs = aMessageWs()
      .withData(aMessageDataPlayerLoginWs().withLogin("ddurieux").build())
      .build();

    const data = playerLoginWs.data as MessageDataPlayerLogin;

    expect(data.login).to.equal("ddurieux");
  });

  it("build a ws player message login with ddurieux login", () => {
    console.log(aMessagePlayerLoginWs().build()); // full random
    console.log(aMessagePlayerLoginWs().build()); // another random
    console.log(
      aMessagePlayerLoginWs()
        .withData({ login: "plop", password: "plip" }) // Avoid because it is difficult to maintain. Use aMessageDataPlayerLoginWs.
        .build()
    );

    const playerLoginWs = aMessagePlayerLoginWs()
      .withData(aMessageDataPlayerLoginWs().withLogin("ddurieux").build())
      .build();

    console.log(playerLoginWs);

    expect(playerLoginWs.data.login).to.equal("ddurieux");
  });
});
