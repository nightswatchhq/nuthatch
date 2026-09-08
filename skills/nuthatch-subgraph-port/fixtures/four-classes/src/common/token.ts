import { Address, BigInt } from '@graphprotocol/graph-ts'
import { ERC20 } from '../../generated/Factory/ERC20'

export function fetchTokenSymbol(tokenAddress: Address): string {
  let contract = ERC20.bind(tokenAddress)
  let result = contract.try_symbol()
  if (!result.reverted) {
    return result.value
  }
  return 'unknown'
}

export function fetchTokenDecimals(tokenAddress: Address): BigInt {
  let contract = ERC20.bind(tokenAddress)
  let result = contract.try_decimals()
  if (!result.reverted) {
    return BigInt.fromI32(result.value)
  }
  return BigInt.fromI32(18)
}
