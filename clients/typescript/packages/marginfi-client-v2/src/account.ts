import { Address, BorshCoder, translateAddress } from "@project-serum/anchor";
import { AccountInfo, Commitment, PublicKey } from "@solana/web3.js";
import { DEFAULT_COMMITMENT, MarginfiClient } from ".";
import Bank, { BankData } from "./bank";
import MarginfiGroup from "./group";
import { MARGINFI_IDL } from "./idl";
import { AccountType, MarginfiConfig, MarginfiProgram } from "./types";

/**
 * Wrapper class around a specific marginfi marginfi account.
 */
class MarginfiAccount {
  public readonly publicKey: PublicKey;
  public group: MarginfiGroup;

  private _authority: PublicKey;

  /**
   * @internal
   */
  private constructor(
    marginfiAccountPk: Address,
    readonly client: MarginfiClient,
    group: MarginfiGroup,
    authority: PublicKey
  ) {
    const publicKey = translateAddress(marginfiAccountPk);
    this.publicKey = publicKey;

    this.group = group;
    this._authority = authority;

    this;
  }

  // --- Getters / Setters

  /**
   * Marginfi account authority address
   */
  get authority(): PublicKey {
    return this._authority;
  }

  /** @internal */
  private get _program() {
    return this.client.program;
  }

  /** @internal */
  private get _config() {
    return this.client.config;
  }

  // --- Factories

  /**
   * MarginfiAccount network factory
   *
   * Fetch account data according to the config and instantiate the corresponding MarginfiAccount.
   *
   * @param marginfiAccountPk Address of the target account
   * @param client marginfi client
   * @returns MarginfiAccount instance
   */
  static async fetch(
    marginfiAccountPk: Address,
    client: MarginfiClient,
    commitment?: Commitment
  ): Promise<MarginfiAccount> {
    const { config, program } = client;
    const _marginfiAccountPk = translateAddress(marginfiAccountPk);

    const accountData = await MarginfiAccount._fetchAccountData(
      _marginfiAccountPk,
      config,
      program,
      commitment
    );

    const marginfiAccount = new MarginfiAccount(
      _marginfiAccountPk,
      client,
      await MarginfiGroup.fetch(config, program, commitment),
      accountData.authority
    );

    require("debug")("mfi:margin-account")(
      "Loaded marginfi account %s",
      _marginfiAccountPk
    );

    return marginfiAccount;
  }

  /**
   * MarginfiAccount local factory (decoded)
   *
   * Instantiate a MarginfiAccount according to the provided decoded data.
   * Check sanity against provided config.
   *
   * @param marginfiAccountPk Address of the target account
   * @param client marginfi client
   * @param accountData Decoded marginfi marginfi account data
   * @param marginfiGroup MarginfiGroup instance
   * @returns MarginfiAccount instance
   */
  static fromAccountData(
    marginfiAccountPk: Address,
    client: MarginfiClient,
    accountData: MarginfiAccountData,
    marginfiGroup: MarginfiGroup
  ) {
    if (!accountData.group.equals(client.config.groupPk))
      throw Error(
        `Marginfi account tied to group ${accountData.group.toBase58()}. Expected: ${client.config.groupPk.toBase58()}`
      );

    const _marginfiAccountPk = translateAddress(marginfiAccountPk);

    return new MarginfiAccount(
      _marginfiAccountPk,
      client,
      marginfiGroup,
      accountData.authority
    );
  }

  /**
   * MarginfiAccount local factory (encoded)
   *
   * Instantiate a MarginfiAccount according to the provided encoded data.
   * Check sanity against provided config.
   *
   * @param marginfiAccountPk Address of the target account
   * @param config marginfi config
   * @param program marginfi Anchor program
   * @param marginfiAccountRawData Encoded marginfi marginfi account data
   * @param marginfiGroup MarginfiGroup instance
   * @returns MarginfiAccount instance
   */
  static fromAccountDataRaw(
    marginfiAccountPk: PublicKey,
    client: MarginfiClient,
    marginfiAccountRawData: Buffer,
    marginfiGroup: MarginfiGroup
  ) {
    const marginfiAccountData = MarginfiAccount.decode(marginfiAccountRawData);

    return MarginfiAccount.fromAccountData(
      marginfiAccountPk,
      client,
      marginfiAccountData,
      marginfiGroup
    );
  }

  // --- Others

  /**
   * Fetch marginfi account data.
   * Check sanity against provided config.
   *
   * @param config marginfi config
   * @param program marginfi Anchor program
   * @returns Decoded marginfi account data struct
   */
  private static async _fetchAccountData(
    accountAddress: Address,
    config: MarginfiConfig,
    program: MarginfiProgram,
    commitment?: Commitment
  ): Promise<MarginfiAccountData> {
    const mergedCommitment =
      commitment ??
      program.provider.connection.commitment ??
      DEFAULT_COMMITMENT;

    const data: MarginfiAccountData =
      (await program.account.marginfiAccount.fetch(
        accountAddress,
        mergedCommitment
      )) as any;

    if (!data.group.equals(config.groupPk))
      throw Error(
        `Marginfi account tied to group ${data.group.toBase58()}. Expected: ${config.groupPk.toBase58()}`
      );

    return data;
  }

  /**
   * Decode marginfi account data according to the Anchor IDL.
   *
   * @param encoded Raw data buffer
   * @returns Decoded marginfi account data struct
   */
  static decode(encoded: Buffer): MarginfiAccountData {
    const coder = new BorshCoder(MARGINFI_IDL);
    return coder.accounts.decode(AccountType.MarginfiAccount, encoded);
  }

  /**
   * Decode marginfi account data according to the Anchor IDL.
   *
   * @param decoded Marginfi account data struct
   * @returns Raw data buffer
   */
  static async encode(decoded: MarginfiAccountData): Promise<Buffer> {
    const coder = new BorshCoder(MARGINFI_IDL);
    return await coder.accounts.encode(AccountType.MarginfiAccount, decoded);
  }

  /**
   * Update instance data by fetching and storing the latest on-chain state.
   */
  async reload() {
    require("debug")(`mfi:margin-account:${this.publicKey.toString()}:loader`)(
      "Reloading account data"
    );
    const [marginfiGroupAi, marginfiAccountAi] =
      await this.loadGroupAndAccountAi();
    const marginfiAccountData = MarginfiAccount.decode(marginfiAccountAi.data);
    if (!marginfiAccountData.group.equals(this._config.groupPk))
      throw Error(
        `Marginfi account tied to group ${marginfiAccountData.group.toBase58()}. Expected: ${this._config.groupPk.toBase58()}`
      );

    const bankAddresses = this._config.banks.map((b) => b.address);
    let bankAccountsData = await this._program.account.bank.fetchMultiple(
      bankAddresses
    );

    let nullAccounts = [];
    for (let i = 0; i < bankAccountsData.length; i++) {
      if (bankAccountsData[i] === null) nullAccounts.push(bankAddresses[i]);
    }
    if (nullAccounts.length > 0) {
      throw Error(`Failed to fetch banks ${nullAccounts}`);
    }

    const banks = bankAccountsData.map(
      (bd, index) => new Bank(bankAddresses[index], bd as BankData)
    );

    this.group = MarginfiGroup.fromAccountDataRaw(
      this._config,
      this._program,
      marginfiGroupAi.data,
      banks
    );
    this._updateFromAccountData(marginfiAccountData);
  }

  /**
   * Update instance data from provided data struct.
   *
   * @param data Marginfi account data struct
   */
  private _updateFromAccountData(data: MarginfiAccountData) {
    this._authority = data.authority;
  }

  private async loadGroupAndAccountAi(): Promise<AccountInfo<Buffer>[]> {
    const debug = require("debug")(
      `mfi:margin-account:${this.publicKey.toString()}:loader`
    );
    debug(
      "Loading marginfi account %s, and group %s",
      this.publicKey,
      this._config.groupPk
    );

    let [marginfiGroupAi, marginfiAccountAi] =
      await this.client.provider.connection.getMultipleAccountsInfo(
        [this._config.groupPk, this.publicKey],
        DEFAULT_COMMITMENT
      );

    if (!marginfiAccountAi) {
      throw Error("Marginfi account no found");
    }
    if (!marginfiGroupAi) {
      throw Error("Marginfi Group Account no found");
    }

    return [marginfiGroupAi, marginfiAccountAi];
  }

  // public toString() {
  //   const marginRequirementInit = this.computeMarginRequirement(
  //     MarginRequirementType.Init
  //   );
  //   const marginRequirementMaint = this.computeMarginRequirement(
  //     MarginRequirementType.Maint
  //   );

  //   const initHealth =
  //     marginRequirementInit.toNumber() <= 0
  //       ? Infinity
  //       : equity.div(marginRequirementInit.toNumber());
  //   const maintHealth =
  //     marginRequirementMaint.toNumber() <= 0
  //       ? Infinity
  //       : equity.div(marginRequirementMaint.toNumber());
  //   const marginRatio = liabilities.lte(0) ? Infinity : equity.div(liabilities);

  //   let str = `-----------------
  // Marginfi account:
  //   Address: ${this.publicKey.toBase58()}
  //   GA Balance: ${deposits.toFixed(6)}
  //   Equity: ${equity.toFixed(6)}
  //   Mr Adjusted Equity: ${mrEquity.toFixed(6)}
  //   Assets: ${assets.toFixed(6)},
  //   Liabilities: ${liabilities.toFixed(6)}
  //   Margin ratio: ${marginRatio.toFixed(6)}
  //   Requirement
  //     init: ${marginRequirementInit.toFixed(6)}, health: ${initHealth.toFixed(
  //     6
  //   )}
  //     maint: ${marginRequirementMaint.toFixed(
  //       6
  //     )}, health: ${maintHealth.toFixed(6)}`;

  //   if (this.activeUtps.length > 0) {
  //     str = str.concat("\n-----------------\nUTPs:");
  //   }
  //   for (let utp of this.activeUtps) {
  //     const utpStr = `\n  ${UTP_NAME[utp.index]}:
  //     Address: ${utp.address.toBase58()}
  //     Equity: ${utp.equity.toFixed(6)},
  //     Free collateral: ${utp.freeCollateral.toFixed(6)}`;
  //     str = str.concat(utpStr);
  //   }

  //   return str;
  // }

  // [customInspectSymbol](_depth: number, _inspectOptions: any, _inspect: any) {
  //   return this.toString();
  // }
}

export default MarginfiAccount;

// On-chain types

export interface MarginfiAccountData {
  group: PublicKey;
  authority: PublicKey;
}
