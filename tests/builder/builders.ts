import { Faker } from "@faker-js/faker/.";
import { FakerGeneratorFactory } from "./FakerGeneratorFactory";
import { MessageWsBuilder } from "./webSocket/MessageWsBuilder";
import { MessageDataPlayerLoginBuilder } from "./webSocket/data/MessageDataPlayerLoginBuilder";
import { MessageDataInterface } from "./model/horizon/data/MessageDataInterface";
import { MessageDataPlayerLogin } from "./model/horizon/data/MessageDataPlayerLogin";
import { EventWs, NamespaceWs } from "./model/horizon/MessageWs";

export const aMessageWs = (
  generator?: Faker
): MessageWsBuilder<MessageDataInterface> => {
  const faker = generator ?? FakerGeneratorFactory.getInstance();

  return new MessageWsBuilder(faker);
};

export const aMessagePlayerLoginWs = (
  generator?: Faker
): MessageWsBuilder<MessageDataPlayerLogin> => {
  const faker = generator ?? FakerGeneratorFactory.getInstance();

  return new MessageWsBuilder<MessageDataPlayerLogin>(faker)
    .withEvent(EventWs.INIT)
    .withNamespace(NamespaceWs.PLAYER)
    .withData(aMessageDataPlayerLoginWs().build());
};

export const aMessageDataPlayerLoginWs = (
  generator?: Faker
): MessageDataPlayerLoginBuilder => {
  const faker = generator ?? FakerGeneratorFactory.getInstance();

  return new MessageDataPlayerLoginBuilder(faker);
};
