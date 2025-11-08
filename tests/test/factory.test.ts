import { expect } from "chai";
import { FakerGeneratorFactory } from "../builder/FakerGeneratorFactory";
import { aPlayerLoginWs } from "../builder/builders";

describe("FakerGeneratorFactory", () => {
  it("should use the env seed if provided", () => {
    const faker = FakerGeneratorFactory.getInstance();

    expect(faker).to.exist;
    expect(typeof faker.internet.email()).to.equal("string");
  });

  it("build a ws player message login with ddurieux login", () => {
    // console.log(aPlayerLoginWs().build()); // full random
    // console.log(aPlayerLoginWs().build()); // another random
    // console.log(
    //   aPlayerLoginWs().withLogin("plop").withPassword("Plip").build()
    // );

    const playerLoginWs = aPlayerLoginWs().withLogin("ddurieux").build();

    // console.log(playerLoginWs);

    expect(playerLoginWs.data.login).to.equal("ddurieux");
  });
});
