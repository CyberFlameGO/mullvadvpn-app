import { sprintf } from 'sprintf-js';

import { messages } from '../../shared/gettext';
import { TunnelState } from '../daemon-rpc-types';
import {
  InAppNotification,
  InAppNotificationProvider,
  SystemNotificationProvider,
} from './notification';

interface ConnectingNotificationContext {
  tunnelState: TunnelState;
  reconnecting?: boolean;
}

export class ConnectingNotificationProvider
  implements SystemNotificationProvider, InAppNotificationProvider {
  public constructor(private context: ConnectingNotificationContext) {}

  public mayDisplay() {
    return this.context.tunnelState.state === 'connecting' && !this.context.reconnecting;
  }

  public getSystemNotification() {
    if (this.context.tunnelState.state === 'connecting') {
      let message = messages.pgettext('notifications', 'Connecting');
      const location = this.context.tunnelState.details?.location?.hostname;
      if (location) {
        message = sprintf(
          // TRANSLATORS: The message showed when a server is being connected to.
          // TRANSLATORS: Available placeholder:
          // TRANSLATORS: %(location) - name of the server location we're connecting to (e.g. "se-got-003")
          messages.pgettext('notifications', 'Connecting to %(location)s'),
          {
            location,
          },
        );
      }

      return {
        message,
        critical: false,
      };
    } else {
      return undefined;
    }
  }

  public getInAppNotification(): InAppNotification {
    return {
      title: messages.pgettext('in-app-notifications', 'BLOCKING INTERNET'),
    };
  }
}
