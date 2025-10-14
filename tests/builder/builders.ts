import { Faker } from "@faker-js/faker/.";
import { FakerGeneratorFactory } from "./FakerGeneratorFactory";
import { PlayerLoginWsBuilder } from "./builders/PlayerLoginWsBuilder";

export const aPlayerLoginWs = (generator?: Faker): PlayerLoginWsBuilder => {
  const faker = generator ?? FakerGeneratorFactory.getInstance();

  return new PlayerLoginWsBuilder(faker);
};
