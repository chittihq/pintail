import type { ChartConfig } from '.'
import { isClient } from '@vueuse/core'
import { h, render } from 'vue'

interface Constructor<P = any> {
  __isFragment?: never
  __isTeleport?: never
  __isSuspense?: never
  new (...args: any[]): {
    $props: P
  }
}

export function componentToString<P>(config: ChartConfig, component: Constructor<P>, props?: P) {
  if (!isClient)
    return

  return (_data: any, x: number | Date) => {
    const data = 'data' in _data ? _data.data : _data
    const vnode = h<unknown>(component, { ...props, payload: data, config, x })
    const div = document.createElement('div')
    try {
      render(vnode, div)
      return div.innerHTML
    } finally {
      // Only the markup escapes. Release watchers, hooks and listeners from
      // the detached component before returning; live payloads are not cached.
      render(null, div)
    }
  }
}
