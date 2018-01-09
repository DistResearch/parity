// Copyright 2015-2017 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

/* eslint react/no-unused-prop-types: 0 */

import React, { Component, PropTypes } from 'react';
import AutoComplete from 'material-ui/AutoComplete';

export default class WrappedAutoComplete extends Component {
  render () {
    return (
      <AutoComplete { ...this.props } />
    );
  }

  static defaultProps = {
    openOnFocus: true,
    filter: (searchText, key) => searchText === '' || key.toLowerCase().indexOf(searchText.toLowerCase()) !== -1
  }

  static propTypes = {
    dataSource: PropTypes.array.isRequired,
    filter: PropTypes.func,
    name: PropTypes.string.isRequired,
    openOnFocus: PropTypes.bool
  }

  static contextTypes = {
    muiTheme: PropTypes.object.isRequired
  }
}